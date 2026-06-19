//! Incremental HTTP/1.1 request and response parsers (RFC 9112).
//!
//! Both [`RequestParser`] and [`ResponseParser`] are state machines that
//! accept bytes via `feed()` and produce parsed messages once complete.
//! They handle `Content-Length`, chunked transfer-encoding, and
//! connection-close body framing.

use super::strip_line_ending;
use crate::error::HttpError;

/// Maximum header section size (8 KiB).
const MAX_HEADER_SIZE: usize = 8 * 1024;

/// Maximum chunk-size line length (64 bytes is generous for hex size + extensions).
const MAX_CHUNK_SIZE_LINE: usize = 64;

/// How the message body length is determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// Fixed-length body via Content-Length header.
    ContentLength(u64),
    /// Chunked transfer-encoding.
    Chunked,
    /// Read until the connection closes (no length indicator).
    UntilClose,
    /// No body (HEAD response, 204, 304, etc.).
    None,
}

/// Parser state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    /// Parsing the request line (method SP request-target SP HTTP-version CRLF).
    RequestLine,
    /// Parsing the status line (HTTP-version SP status-code SP reason-phrase CRLF).
    StatusLine,
    /// Parsing header fields.
    HeaderLine,
    /// Reading a fixed-length body.
    Body,
    /// Reading chunk size line.
    ChunkSize,
    /// Reading chunk data.
    ChunkData { remaining: usize },
    /// Reading the CRLF after chunk data.
    ChunkTrailer,
    /// Message is complete.
    Complete,
}

// ─── Response Parser ─────────────────────────────────────────────────

/// Incremental HTTP/1.1 response parser.
pub struct ResponseParser {
    state: ParseState,
    buf: Vec<u8>,
    status: Option<u16>,
    version: Option<u8>, // 0 = HTTP/1.0, 1 = HTTP/1.1
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    body_kind: BodyKind,
    body_remaining: u64,
    header_bytes_read: usize,
    /// True when the zero-length final chunk has been parsed.
    saw_last_chunk: bool,
}

impl ResponseParser {
    pub fn new() -> Self {
        Self {
            state: ParseState::StatusLine,
            buf: Vec::new(),
            status: None,
            version: None,
            headers: Vec::new(),
            body: Vec::new(),
            body_kind: BodyKind::None,
            body_remaining: 0,
            header_bytes_read: 0,
            saw_last_chunk: false,
        }
    }

    /// Feed bytes into the parser. Returns `(consumed, complete)`.
    pub fn feed(&mut self, data: &[u8]) -> Result<(usize, bool), HttpError> {
        let mut pos = 0;
        while pos < data.len() && self.state != ParseState::Complete {
            match self.state {
                ParseState::StatusLine => {
                    pos += self.parse_status_line(&data[pos..])?;
                }
                ParseState::HeaderLine => {
                    pos += self.parse_header_line(&data[pos..])?;
                }
                ParseState::Body => {
                    pos += self.read_body(&data[pos..]);
                }
                ParseState::ChunkSize => {
                    pos += self.parse_chunk_size(&data[pos..])?;
                }
                ParseState::ChunkData { .. } => {
                    pos += self.read_chunk_data(&data[pos..]);
                }
                ParseState::ChunkTrailer => {
                    pos += self.skip_chunk_trailer(&data[pos..]);
                }
                ParseState::Complete => break,
                _ => return Err(HttpError::Parse("unexpected parser state".into())),
            }
        }
        Ok((pos, self.state == ParseState::Complete))
    }

    /// Extract the parsed response. Only valid after `feed()` returns `complete = true`.
    pub fn take_response(&mut self) -> Result<http::Response<Vec<u8>>, HttpError> {
        let status = self
            .status
            .ok_or_else(|| HttpError::Parse("no status code".into()))?;
        let status = http::StatusCode::from_u16(status)
            .map_err(|e| HttpError::Parse(format!("invalid status: {e}")))?;

        let version = match self.version {
            Some(0) => http::Version::HTTP_10,
            Some(1) => http::Version::HTTP_11,
            _ => http::Version::HTTP_11,
        };

        let mut builder = http::Response::builder().status(status).version(version);
        for (name, value) in &self.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }

        let body = std::mem::take(&mut self.body);
        builder
            .body(body)
            .map_err(|e| HttpError::Parse(format!("build response: {e}")))
    }

    /// Whether the connection should be kept alive based on headers and version.
    pub fn keep_alive(&self) -> bool {
        let conn_header = self
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("connection"))
            .map(|(_, v)| v.as_str());

        match self.version {
            Some(1) => {
                // HTTP/1.1: keep-alive by default unless "close"
                !matches!(conn_header, Some(v) if v.eq_ignore_ascii_case("close"))
            }
            _ => {
                // HTTP/1.0: close by default unless "keep-alive"
                matches!(conn_header, Some(v) if v.eq_ignore_ascii_case("keep-alive"))
            }
        }
    }

    /// Parse `Keep-Alive: max=N` from the response headers.
    ///
    /// Returns `Some(N)` if the header is present and contains a valid `max` parameter.
    pub fn keep_alive_max(&self) -> Option<u64> {
        parse_keep_alive_max(&self.headers)
    }

    /// Returns true once the status line and all headers have been parsed.
    pub fn is_head_complete(&self) -> bool {
        self.status.is_some()
            && matches!(
                self.state,
                ParseState::Body
                    | ParseState::ChunkSize
                    | ParseState::ChunkData { .. }
                    | ParseState::ChunkTrailer
                    | ParseState::Complete
            )
    }

    /// Mark the response complete without reading a message body.
    pub fn complete_without_body(&mut self) {
        if !self.is_head_complete() {
            return;
        }

        self.buf.clear();
        self.body.clear();
        self.body_kind = BodyKind::None;
        self.body_remaining = 0;
        self.saw_last_chunk = false;
        self.state = ParseState::Complete;
    }

    // ── Internal parsing methods ──

    fn parse_status_line(&mut self, data: &[u8]) -> Result<usize, HttpError> {
        for (i, &b) in data.iter().enumerate() {
            self.buf.push(b);
            self.header_bytes_read += 1;
            if self.header_bytes_read > MAX_HEADER_SIZE {
                return Err(HttpError::HeaderTooLarge);
            }
            if let Some(content) = strip_line_ending(&self.buf) {
                // Parse "HTTP/1.x SP status SP reason CRLF"
                let line = std::str::from_utf8(content)
                    .map_err(|_| HttpError::Parse("invalid UTF-8 in status line".into()))?;

                if line.len() < 12 || !line.starts_with("HTTP/1.") {
                    return Err(HttpError::Parse(format!("invalid status line: {line}")));
                }

                let version_byte = line.as_bytes()[7];
                if version_byte != b'0' && version_byte != b'1' {
                    return Err(HttpError::Parse(format!(
                        "unsupported HTTP version: HTTP/1.{}",
                        version_byte as char
                    )));
                }
                self.version = Some(version_byte - b'0');
                let status_str = &line[9..12];
                self.status = Some(
                    status_str
                        .parse::<u16>()
                        .map_err(|_| HttpError::Parse(format!("invalid status: {status_str}")))?,
                );

                self.buf.clear();
                self.state = ParseState::HeaderLine;
                return Ok(i + 1);
            }
        }
        Ok(data.len())
    }

    fn parse_header_line(&mut self, data: &[u8]) -> Result<usize, HttpError> {
        for (i, &b) in data.iter().enumerate() {
            self.buf.push(b);
            self.header_bytes_read += 1;
            if self.header_bytes_read > MAX_HEADER_SIZE {
                return Err(HttpError::HeaderTooLarge);
            }
            if let Some(content) = strip_line_ending(&self.buf) {
                if content.is_empty() {
                    // Empty line = end of headers
                    self.buf.clear();
                    self.determine_body_kind()?;
                    return Ok(i + 1);
                }

                let line = std::str::from_utf8(content)
                    .map_err(|_| HttpError::Parse("invalid UTF-8 in header".into()))?;

                if let Some((name, value)) = line.split_once(':') {
                    self.headers
                        .push((name.trim().to_string(), value.trim().to_string()));
                } else {
                    return Err(HttpError::Parse(format!("malformed header: {line}")));
                }

                self.buf.clear();
                return Ok(i + 1);
            }
        }
        Ok(data.len())
    }

    fn determine_body_kind(&mut self) -> Result<(), HttpError> {
        // Check for informational, 204, 304 — no body
        if let Some(status) = self.status {
            if (100..200).contains(&status) || status == 204 || status == 304 {
                self.body_kind = BodyKind::None;
                self.state = ParseState::Complete;
                return Ok(());
            }
        }

        // Check transfer-encoding
        let is_chunked = has_final_chunked_transfer_coding(&self.headers);
        if is_chunked {
            self.body_kind = BodyKind::Chunked;
            self.state = ParseState::ChunkSize;
            return Ok(());
        }

        // Check content-length
        if let Some(len) = parse_content_length(&self.headers)? {
            if len == 0 {
                self.body_kind = BodyKind::None;
                self.state = ParseState::Complete;
            } else {
                self.body_kind = BodyKind::ContentLength(len);
                self.body_remaining = len;
                self.state = ParseState::Body;
            }
            return Ok(());
        }

        // No length indicator — read until close
        self.body_kind = BodyKind::UntilClose;
        self.state = ParseState::Body;
        Ok(())
    }

    fn read_body(&mut self, data: &[u8]) -> usize {
        match self.body_kind {
            BodyKind::ContentLength(_) => {
                let want = self.body_remaining.min(data.len() as u64) as usize;
                self.body.extend_from_slice(&data[..want]);
                self.body_remaining -= want as u64;
                if self.body_remaining == 0 {
                    self.state = ParseState::Complete;
                }
                want
            }
            BodyKind::UntilClose => {
                // Caller signals EOF by providing 0 bytes or calling a separate method
                self.body.extend_from_slice(data);
                data.len()
            }
            _ => 0,
        }
    }

    fn parse_chunk_size(&mut self, data: &[u8]) -> Result<usize, HttpError> {
        for (i, &b) in data.iter().enumerate() {
            self.buf.push(b);
            if self.buf.len() > MAX_CHUNK_SIZE_LINE {
                return Err(HttpError::InvalidChunkEncoding);
            }
            if let Some(content) = strip_line_ending(&self.buf) {
                let line =
                    std::str::from_utf8(content).map_err(|_| HttpError::InvalidChunkEncoding)?;
                // Chunk size may have extensions after ';'
                let size_str = line.split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size_str, 16)
                    .map_err(|_| HttpError::InvalidChunkEncoding)?;

                self.buf.clear();
                if size == 0 {
                    self.saw_last_chunk = true;
                    self.state = ParseState::ChunkTrailer;
                } else {
                    self.state = ParseState::ChunkData { remaining: size };
                }
                return Ok(i + 1);
            }
        }
        Ok(data.len())
    }

    fn read_chunk_data(&mut self, data: &[u8]) -> usize {
        if let ParseState::ChunkData { remaining } = &mut self.state {
            let take = data.len().min(*remaining);
            self.body.extend_from_slice(&data[..take]);
            *remaining -= take;
            if *remaining == 0 {
                self.state = ParseState::ChunkTrailer;
            }
            take
        } else {
            0
        }
    }

    fn skip_chunk_trailer(&mut self, data: &[u8]) -> usize {
        for (i, &b) in data.iter().enumerate() {
            self.buf.push(b);
            if strip_line_ending(&self.buf).is_some() {
                self.buf.clear();
                if self.saw_last_chunk {
                    self.state = ParseState::Complete;
                } else {
                    self.state = ParseState::ChunkSize;
                }
                return i + 1;
            }
        }
        data.len()
    }

    /// Signal end-of-stream (for `UntilClose` body kind).
    pub fn signal_eof(&mut self) {
        if self.body_kind == BodyKind::UntilClose {
            self.state = ParseState::Complete;
        }
    }

    /// Returns true if parsing is complete.
    pub fn is_complete(&self) -> bool {
        self.state == ParseState::Complete
    }
}

impl Default for ResponseParser {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Request Parser ──────────────────────────────────────────────────

/// Incremental HTTP/1.1 request parser.
pub struct RequestParser {
    state: ParseState,
    buf: Vec<u8>,
    method: Option<String>,
    uri: Option<String>,
    version: Option<u8>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    body_kind: BodyKind,
    body_remaining: u64,
    header_bytes_read: usize,
    saw_last_chunk: bool,
}

impl RequestParser {
    pub fn new() -> Self {
        Self {
            state: ParseState::RequestLine,
            buf: Vec::new(),
            method: None,
            uri: None,
            version: None,
            headers: Vec::new(),
            body: Vec::new(),
            body_kind: BodyKind::None,
            body_remaining: 0,
            header_bytes_read: 0,
            saw_last_chunk: false,
        }
    }

    /// Feed bytes into the parser. Returns `(consumed, complete)`.
    pub fn feed(&mut self, data: &[u8]) -> Result<(usize, bool), HttpError> {
        let mut pos = 0;
        while pos < data.len() && self.state != ParseState::Complete {
            match self.state {
                ParseState::RequestLine => {
                    pos += self.parse_request_line(&data[pos..])?;
                }
                ParseState::HeaderLine => {
                    pos += self.parse_header_line(&data[pos..])?;
                }
                ParseState::Body => {
                    pos += self.read_body(&data[pos..]);
                }
                ParseState::ChunkSize => {
                    pos += self.parse_chunk_size(&data[pos..])?;
                }
                ParseState::ChunkData { .. } => {
                    pos += self.read_chunk_data(&data[pos..]);
                }
                ParseState::ChunkTrailer => {
                    pos += self.skip_chunk_trailer(&data[pos..]);
                }
                ParseState::Complete => break,
                _ => return Err(HttpError::Parse("unexpected parser state".into())),
            }
        }
        Ok((pos, self.state == ParseState::Complete))
    }

    /// Extract the parsed request. Only valid after `feed()` returns `complete = true`.
    pub fn take_request(&mut self) -> Result<http::Request<Vec<u8>>, HttpError> {
        let method = self
            .method
            .take()
            .ok_or_else(|| HttpError::Parse("no method".into()))?;
        let method: http::Method = method
            .parse()
            .map_err(|e| HttpError::Parse(format!("invalid method: {e}")))?;

        let uri = self
            .uri
            .take()
            .ok_or_else(|| HttpError::Parse("no URI".into()))?;
        let uri: http::Uri = uri
            .parse()
            .map_err(|e| HttpError::Parse(format!("invalid URI: {e}")))?;

        let version = match self.version {
            Some(0) => http::Version::HTTP_10,
            Some(1) => http::Version::HTTP_11,
            _ => http::Version::HTTP_11,
        };

        let mut builder = http::Request::builder()
            .method(method)
            .uri(uri)
            .version(version);
        for (name, value) in &self.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }

        let body = std::mem::take(&mut self.body);
        builder
            .body(body)
            .map_err(|e| HttpError::Parse(format!("build request: {e}")))
    }

    /// Reset parser for the next request on a keep-alive connection.
    pub fn reset(&mut self) {
        self.state = ParseState::RequestLine;
        self.buf.clear();
        self.method = None;
        self.uri = None;
        self.version = None;
        self.headers.clear();
        self.body.clear();
        self.body_kind = BodyKind::None;
        self.body_remaining = 0;
        self.header_bytes_read = 0;
        self.saw_last_chunk = false;
    }

    /// Whether the connection should be kept alive based on headers and version.
    pub fn keep_alive(&self) -> bool {
        let conn_header = self
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("connection"))
            .map(|(_, v)| v.as_str());

        match self.version {
            Some(1) => !matches!(conn_header, Some(v) if v.eq_ignore_ascii_case("close")),
            _ => {
                matches!(conn_header, Some(v) if v.eq_ignore_ascii_case("keep-alive"))
            }
        }
    }

    /// Parse `Keep-Alive: max=N` from the request headers.
    pub fn keep_alive_max(&self) -> Option<u64> {
        parse_keep_alive_max(&self.headers)
    }

    /// Returns true if parsing is complete.
    pub fn is_complete(&self) -> bool {
        self.state == ParseState::Complete
    }

    // ── Internal parsing methods ──

    fn parse_request_line(&mut self, data: &[u8]) -> Result<usize, HttpError> {
        for (i, &b) in data.iter().enumerate() {
            self.buf.push(b);
            self.header_bytes_read += 1;
            if self.header_bytes_read > MAX_HEADER_SIZE {
                return Err(HttpError::HeaderTooLarge);
            }
            if let Some(content) = strip_line_ending(&self.buf) {
                let line = std::str::from_utf8(content)
                    .map_err(|_| HttpError::Parse("invalid UTF-8 in request line".into()))?;

                // "METHOD SP URI SP HTTP/1.x"
                let mut parts = line.splitn(3, ' ');
                let method = parts
                    .next()
                    .ok_or_else(|| HttpError::Parse("missing method".into()))?;
                let uri = parts
                    .next()
                    .ok_or_else(|| HttpError::Parse("missing URI".into()))?;
                let version_str = parts
                    .next()
                    .ok_or_else(|| HttpError::Parse("missing HTTP version".into()))?;

                if !version_str.starts_with("HTTP/1.") {
                    return Err(HttpError::Parse(format!(
                        "unsupported version: {version_str}"
                    )));
                }

                let version_byte = version_str.as_bytes()[7];
                if version_byte != b'0' && version_byte != b'1' {
                    return Err(HttpError::Parse(format!(
                        "unsupported HTTP version: HTTP/1.{}",
                        version_byte as char
                    )));
                }

                self.method = Some(method.to_string());
                self.uri = Some(uri.to_string());
                self.version = Some(version_byte - b'0');

                self.buf.clear();
                self.state = ParseState::HeaderLine;
                return Ok(i + 1);
            }
        }
        Ok(data.len())
    }

    fn parse_header_line(&mut self, data: &[u8]) -> Result<usize, HttpError> {
        for (i, &b) in data.iter().enumerate() {
            self.buf.push(b);
            self.header_bytes_read += 1;
            if self.header_bytes_read > MAX_HEADER_SIZE {
                return Err(HttpError::HeaderTooLarge);
            }
            if let Some(content) = strip_line_ending(&self.buf) {
                if content.is_empty() {
                    self.buf.clear();
                    self.determine_body_kind()?;
                    return Ok(i + 1);
                }

                let line = std::str::from_utf8(content)
                    .map_err(|_| HttpError::Parse("invalid UTF-8 in header".into()))?;

                if let Some((name, value)) = line.split_once(':') {
                    self.headers
                        .push((name.trim().to_string(), value.trim().to_string()));
                } else {
                    return Err(HttpError::Parse(format!("malformed header: {line}")));
                }

                self.buf.clear();
                return Ok(i + 1);
            }
        }
        Ok(data.len())
    }

    fn determine_body_kind(&mut self) -> Result<(), HttpError> {
        // Check transfer-encoding
        let is_chunked = has_final_chunked_transfer_coding(&self.headers);
        if is_chunked {
            self.body_kind = BodyKind::Chunked;
            self.state = ParseState::ChunkSize;
            return Ok(());
        }

        // Check content-length
        if let Some(len) = parse_content_length(&self.headers)? {
            if len == 0 {
                self.body_kind = BodyKind::None;
                self.state = ParseState::Complete;
            } else {
                self.body_kind = BodyKind::ContentLength(len);
                self.body_remaining = len;
                self.state = ParseState::Body;
            }
            return Ok(());
        }

        // No body for requests without Content-Length or Transfer-Encoding
        self.body_kind = BodyKind::None;
        self.state = ParseState::Complete;
        Ok(())
    }

    fn read_body(&mut self, data: &[u8]) -> usize {
        let want = self.body_remaining.min(data.len() as u64) as usize;
        self.body.extend_from_slice(&data[..want]);
        self.body_remaining -= want as u64;
        if self.body_remaining == 0 {
            self.state = ParseState::Complete;
        }
        want
    }

    fn parse_chunk_size(&mut self, data: &[u8]) -> Result<usize, HttpError> {
        for (i, &b) in data.iter().enumerate() {
            self.buf.push(b);
            if self.buf.len() > MAX_CHUNK_SIZE_LINE {
                return Err(HttpError::InvalidChunkEncoding);
            }
            if let Some(content) = strip_line_ending(&self.buf) {
                let line =
                    std::str::from_utf8(content).map_err(|_| HttpError::InvalidChunkEncoding)?;
                let size_str = line.split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size_str, 16)
                    .map_err(|_| HttpError::InvalidChunkEncoding)?;

                self.buf.clear();
                if size == 0 {
                    self.saw_last_chunk = true;
                    self.state = ParseState::ChunkTrailer;
                } else {
                    self.state = ParseState::ChunkData { remaining: size };
                }
                return Ok(i + 1);
            }
        }
        Ok(data.len())
    }

    fn read_chunk_data(&mut self, data: &[u8]) -> usize {
        if let ParseState::ChunkData { remaining } = &mut self.state {
            let take = data.len().min(*remaining);
            self.body.extend_from_slice(&data[..take]);
            *remaining -= take;
            if *remaining == 0 {
                self.state = ParseState::ChunkTrailer;
            }
            take
        } else {
            0
        }
    }

    fn skip_chunk_trailer(&mut self, data: &[u8]) -> usize {
        for (i, &b) in data.iter().enumerate() {
            self.buf.push(b);
            if strip_line_ending(&self.buf).is_some() {
                self.buf.clear();
                if self.saw_last_chunk {
                    self.state = ParseState::Complete;
                } else {
                    self.state = ParseState::ChunkSize;
                }
                return i + 1;
            }
        }
        data.len()
    }
}

impl Default for RequestParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the `max` parameter from a `Keep-Alive` header value.
///
/// The header format is `Keep-Alive: timeout=5, max=100`.
/// Returns `Some(N)` if a valid `max=N` parameter is found.
/// Handles arbitrary whitespace around `=` and is case-insensitive.
fn parse_keep_alive_max(headers: &[(String, String)]) -> Option<u64> {
    let ka_value = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("keep-alive"))
        .map(|(_, v)| v.as_str())?;

    for param in ka_value.split(',') {
        let param = param.trim();
        if let Some((key, val)) = param.split_once('=') {
            if key.trim().eq_ignore_ascii_case("max") {
                if let Ok(n) = val.trim().parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Returns true iff `chunked` is the FINAL transfer-coding.
///
/// RFC 9112 §6.1 / RFC 7230 §3.2.2: multiple `Transfer-Encoding` headers are
/// combined in wire order into one comma-separated list, and a body is chunked
/// only when `chunked` is the last coding. Must NOT match `chunked` elsewhere,
/// nor require the value to be exactly "chunked" — `gzip, chunked` is chunked,
/// but `chunked, gzip` is not.
fn has_final_chunked_transfer_coding(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .rfind(|coding| !coding.is_empty())
        .is_some_and(|coding| coding.eq_ignore_ascii_case("chunked"))
}

/// Parse and validate the message `Content-Length`.
///
/// RFC 9110 §8.6: if multiple `Content-Length` values are present (repeated
/// headers or a comma-separated list) and they disagree, the message MUST be
/// rejected. Do NOT "take the first value" — that is a request-smuggling vector,
/// since an intermediary may act on a different value than this parser. Repeated
/// identical values are tolerated.
fn parse_content_length(headers: &[(String, String)]) -> Result<Option<u64>, HttpError> {
    let mut parsed = None;

    for value in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.as_str())
    {
        for raw in value.split(',') {
            let raw = raw.trim();
            let len = raw
                .parse::<u64>()
                .map_err(|_| HttpError::Parse(format!("invalid Content-Length: {value}")))?;

            if let Some(existing) = parsed {
                if existing != len {
                    return Err(HttpError::Parse(format!(
                        "conflicting Content-Length headers: {existing} vs {len}",
                    )));
                }
            } else {
                parsed = Some(len);
            }
        }
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_response() {
        let mut parser = ResponseParser::new();
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let (consumed, complete) = parser.feed(data).unwrap();
        assert_eq!(consumed, data.len());
        assert!(complete);

        let resp = parser.take_response().unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.body(), b"hello");
    }

    #[test]
    fn parse_chunked_response() {
        let mut parser = ResponseParser::new();
        let data = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (consumed, complete) = parser.feed(data).unwrap();
        assert_eq!(consumed, data.len());
        assert!(complete);

        let resp = parser.take_response().unwrap();
        assert_eq!(resp.body(), b"hello");
    }

    #[test]
    fn transfer_encoding_headers_are_combined_before_final_chunked_check() {
        assert!(!has_final_chunked_transfer_coding(&[
            ("Transfer-Encoding".into(), "chunked".into()),
            ("Transfer-Encoding".into(), "gzip".into()),
        ]));
        assert!(has_final_chunked_transfer_coding(&[
            ("Transfer-Encoding".into(), "gzip".into()),
            ("Transfer-Encoding".into(), "chunked".into()),
        ]));
    }

    #[test]
    fn parse_chunked_response_with_transfer_coding_list() {
        let mut parser = ResponseParser::new();
        let data =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (consumed, complete) = parser.feed(data).unwrap();
        assert_eq!(consumed, data.len());
        assert!(complete);

        let resp = parser.take_response().unwrap();
        assert_eq!(resp.body(), b"hello");
    }

    #[test]
    fn parse_no_body_204() {
        let mut parser = ResponseParser::new();
        let data = b"HTTP/1.1 204 No Content\r\n\r\n";
        let (_, complete) = parser.feed(data).unwrap();
        assert!(complete);

        let resp = parser.take_response().unwrap();
        assert_eq!(resp.status(), 204);
        assert!(resp.body().is_empty());
    }

    #[test]
    fn parse_incremental_feed() {
        let mut parser = ResponseParser::new();

        let (_, complete) = parser.feed(b"HTTP/1.1 200 OK\r\n").unwrap();
        assert!(!complete);

        let (_, complete) = parser.feed(b"Content-Length: 3\r\n\r\n").unwrap();
        assert!(!complete);

        let (_, complete) = parser.feed(b"abc").unwrap();
        assert!(complete);

        let resp = parser.take_response().unwrap();
        assert_eq!(resp.body(), b"abc");
    }

    #[test]
    fn parse_simple_request() {
        let mut parser = RequestParser::new();
        let data = b"GET /index.html HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let (consumed, complete) = parser.feed(data).unwrap();
        assert_eq!(consumed, data.len());
        assert!(complete);

        let req = parser.take_request().unwrap();
        assert_eq!(req.method(), http::Method::GET);
        assert_eq!(req.uri(), "/index.html");
        assert_eq!(
            req.headers().get("Host").unwrap().to_str().unwrap(),
            "example.com"
        );
    }

    #[test]
    fn parse_post_with_body() {
        let mut parser = RequestParser::new();
        let data = b"POST /api HTTP/1.1\r\nContent-Length: 13\r\n\r\n{\"key\":\"val\"}";
        let (_, complete) = parser.feed(data).unwrap();
        assert!(complete);

        let req = parser.take_request().unwrap();
        assert_eq!(req.method(), http::Method::POST);
        assert_eq!(req.body(), b"{\"key\":\"val\"}");
    }

    #[test]
    fn parse_chunked_request_with_transfer_coding_list() {
        let mut parser = RequestParser::new();
        let data =
            b"POST /upload HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (consumed, complete) = parser.feed(data).unwrap();
        assert_eq!(consumed, data.len());
        assert!(complete);

        let req = parser.take_request().unwrap();
        assert_eq!(req.method(), http::Method::POST);
        assert_eq!(req.body(), b"hello");
    }

    #[test]
    fn header_too_large() {
        let mut parser = RequestParser::new();
        let huge_line = format!(
            "GET / HTTP/1.1\r\nX-Big: {}\r\n\r\n",
            "A".repeat(MAX_HEADER_SIZE)
        );
        let result = parser.feed(huge_line.as_bytes());
        assert!(matches!(result, Err(HttpError::HeaderTooLarge)));
    }

    #[test]
    fn keep_alive_http11() {
        let mut parser = ResponseParser::new();
        parser
            .feed(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        assert!(parser.keep_alive());
    }

    #[test]
    fn keep_alive_close() {
        let mut parser = ResponseParser::new();
        parser
            .feed(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        assert!(!parser.keep_alive());
    }

    #[test]
    fn parse_response_bare_lf() {
        let mut parser = ResponseParser::new();
        let data = b"HTTP/1.1 200 OK\nContent-Length: 5\n\nhello";
        let (consumed, complete) = parser.feed(data).unwrap();
        assert_eq!(consumed, data.len());
        assert!(complete);

        let resp = parser.take_response().unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.body(), b"hello");
    }

    #[test]
    fn parse_request_bare_lf() {
        let mut parser = RequestParser::new();
        let data = b"GET /index.html HTTP/1.1\nHost: example.com\n\n";
        let (consumed, complete) = parser.feed(data).unwrap();
        assert_eq!(consumed, data.len());
        assert!(complete);

        let req = parser.take_request().unwrap();
        assert_eq!(req.method(), http::Method::GET);
        assert_eq!(req.uri(), "/index.html");
    }

    #[test]
    fn parse_chunked_bare_lf() {
        let mut parser = ResponseParser::new();
        let data = b"HTTP/1.1 200 OK\nTransfer-Encoding: chunked\n\n5\nhello\n0\n\n";
        let (consumed, complete) = parser.feed(data).unwrap();
        assert_eq!(consumed, data.len());
        assert!(complete);

        let resp = parser.take_response().unwrap();
        assert_eq!(resp.body(), b"hello");
    }

    #[test]
    fn response_rejects_invalid_version_byte() {
        let mut parser = ResponseParser::new();
        let data = b"HTTP/1.9 200 OK\r\nContent-Length: 0\r\n\r\n";
        let result = parser.feed(data);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, HttpError::Parse(msg) if msg.contains("unsupported HTTP version")));
    }

    #[test]
    fn response_accepts_http10() {
        let mut parser = ResponseParser::new();
        let data = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let (_, complete) = parser.feed(data).unwrap();
        assert!(complete);
        let resp = parser.take_response().unwrap();
        assert_eq!(resp.version(), http::Version::HTTP_10);
    }

    #[test]
    fn request_rejects_invalid_version_byte() {
        let mut parser = RequestParser::new();
        let data = b"GET / HTTP/1.2\r\nHost: x\r\n\r\n";
        let result = parser.feed(data);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, HttpError::Parse(msg) if msg.contains("unsupported HTTP version")));
    }

    #[test]
    fn request_accepts_http10() {
        let mut parser = RequestParser::new();
        let data = b"GET / HTTP/1.0\r\nHost: x\r\n\r\n";
        let (_, complete) = parser.feed(data).unwrap();
        assert!(complete);
        let req = parser.take_request().unwrap();
        assert_eq!(req.version(), http::Version::HTTP_10);
    }

    #[test]
    fn response_rejects_invalid_content_length() {
        let mut parser = ResponseParser::new();
        let result = parser.feed(b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\n");
        assert!(
            matches!(result, Err(HttpError::Parse(msg)) if msg.contains("invalid Content-Length"))
        );
    }

    #[test]
    fn request_rejects_invalid_content_length() {
        let mut parser = RequestParser::new();
        let result = parser.feed(b"POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n");
        assert!(
            matches!(result, Err(HttpError::Parse(msg)) if msg.contains("invalid Content-Length"))
        );
    }

    #[test]
    fn response_rejects_conflicting_duplicate_content_length_headers() {
        let mut parser = ResponseParser::new();
        let result =
            parser.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 10\r\n\r\nhello");
        assert!(
            matches!(result, Err(HttpError::Parse(msg)) if msg.contains("conflicting Content-Length"))
        );
    }

    #[test]
    fn request_rejects_conflicting_duplicate_content_length_headers() {
        let mut parser = RequestParser::new();
        let result =
            parser.feed(b"POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 10\r\n\r\n");
        assert!(
            matches!(result, Err(HttpError::Parse(msg)) if msg.contains("conflicting Content-Length"))
        );
    }

    #[test]
    fn response_accepts_matching_duplicate_content_length_headers() {
        let mut parser = ResponseParser::new();
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello";
        let (_, complete) = parser.feed(data).unwrap();
        assert!(complete);

        let resp = parser.take_response().unwrap();
        assert_eq!(resp.body(), b"hello");
    }

    #[test]
    fn response_accepts_matching_comma_separated_content_length_values() {
        let mut parser = ResponseParser::new();
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 5, 5\r\n\r\nhello";
        let (_, complete) = parser.feed(data).unwrap();
        assert!(complete);

        let resp = parser.take_response().unwrap();
        assert_eq!(resp.body(), b"hello");
    }

    #[test]
    fn keep_alive_max_parsed() {
        let mut parser = ResponseParser::new();
        parser
            .feed(b"HTTP/1.1 200 OK\r\nKeep-Alive: timeout=5, max=100\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        assert_eq!(parser.keep_alive_max(), Some(100));
    }

    #[test]
    fn keep_alive_max_missing() {
        let mut parser = ResponseParser::new();
        parser
            .feed(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        assert_eq!(parser.keep_alive_max(), None);
    }

    #[test]
    fn keep_alive_max_only() {
        let mut parser = ResponseParser::new();
        parser
            .feed(b"HTTP/1.1 200 OK\r\nKeep-Alive: max=50\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        assert_eq!(parser.keep_alive_max(), Some(50));
    }

    #[test]
    fn keep_alive_max_spaces_around_equals() {
        let mut parser = ResponseParser::new();
        parser
            .feed(
                b"HTTP/1.1 200 OK\r\nKeep-Alive: timeout=5, max = 100\r\nContent-Length: 0\r\n\r\n",
            )
            .unwrap();
        assert_eq!(parser.keep_alive_max(), Some(100));
    }

    #[test]
    fn keep_alive_max_case_insensitive() {
        let mut parser = ResponseParser::new();
        parser
            .feed(b"HTTP/1.1 200 OK\r\nKeep-Alive: MAX=75\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        assert_eq!(parser.keep_alive_max(), Some(75));
    }

    #[test]
    fn parser_1xx_consumed_bytes_correct() {
        // Simulate 100 Continue + 200 OK in the same buffer
        let data = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

        let mut parser = ResponseParser::new();
        let (consumed, complete) = parser.feed(data).unwrap();

        // Parser completes at the 100 boundary
        assert!(complete);
        let resp = parser.take_response().unwrap();
        assert_eq!(resp.status(), 100);
        assert!(resp.body().is_empty());

        // Remaining bytes should parse as the 200 response
        assert!(consumed < data.len());
        let mut parser2 = ResponseParser::new();
        let (consumed2, complete2) = parser2.feed(&data[consumed..]).unwrap();
        assert!(complete2);
        assert_eq!(consumed2, data.len() - consumed);

        let resp2 = parser2.take_response().unwrap();
        assert_eq!(resp2.status(), 200);
        assert_eq!(resp2.body(), b"ok");
    }

    #[test]
    fn chunk_size_line_too_long_rejected() {
        let mut parser = ResponseParser::new();
        parser
            .feed(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap();
        // Feed a chunk-size line that exceeds the limit
        let huge_line = "A".repeat(MAX_CHUNK_SIZE_LINE + 10);
        let result = parser.feed(huge_line.as_bytes());
        assert!(matches!(result, Err(HttpError::InvalidChunkEncoding)));
    }
}
