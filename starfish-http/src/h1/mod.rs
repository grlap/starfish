//! HTTP/1.1 implementation (RFC 9112).
//!
//! Modules:
//! - [`parser`] — incremental request and response parsing
//! - [`encoder`] — request and response serialization
//! - [`chunked`] — chunked transfer-encoding codec
//! - [`connection`] — keep-alive connection management
//! - [`client`] — HTTP/1.1 client
//! - [`server`] — HTTP/1.1 server

pub mod chunked;
pub mod client;
pub mod connection;
pub mod encoder;
pub mod parser;
pub mod server;

/// Strip a line ending from a buffer: removes trailing `\n` and optional `\r\n`.
/// Returns the content before the line ending, or `None` if buffer doesn't end with `\n`.
pub(crate) fn strip_line_ending(buf: &[u8]) -> Option<&[u8]> {
    if buf.last() != Some(&b'\n') {
        return None;
    }
    let end = buf.len() - 1;
    if end > 0 && buf[end - 1] == b'\r' {
        Some(&buf[..end - 1])
    } else {
        Some(&buf[..end])
    }
}
