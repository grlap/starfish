//! HTTP/1.1 request and response serialization (RFC 9112).
//!
//! Encodes `http::Request` and `http::Response` into wire-format bytes.

use std::fmt::Write as FmtWrite;

fn has_transfer_encoding(headers: &http::HeaderMap) -> bool {
    headers.contains_key(http::header::TRANSFER_ENCODING)
}

/// Encode an HTTP/1.1 request into wire bytes.
///
/// Format: `METHOD SP URI SP HTTP/1.1 CRLF headers CRLF body`
pub fn encode_request(req: &http::Request<Vec<u8>>) -> Vec<u8> {
    let mut buf = String::with_capacity(256);
    let has_transfer_encoding = has_transfer_encoding(req.headers());

    // Request line
    let _ = write!(buf, "{} {} HTTP/1.1\r\n", req.method(), req.uri());

    // Headers
    for (name, value) in req.headers() {
        // RFC 9112 §6.1: a message with both Transfer-Encoding and Content-Length is
        // invalid (and a request-smuggling risk). Transfer-Encoding is authoritative:
        // suppress the auto-added Content-Length AND strip any caller-supplied one.
        if has_transfer_encoding && name == http::header::CONTENT_LENGTH {
            continue;
        }
        let _ = write!(
            buf,
            "{}: {}\r\n",
            name.as_str(),
            value.to_str().unwrap_or("")
        );
    }

    // If no Content-Length and there's a body, add it
    if !has_transfer_encoding
        && !req.headers().contains_key(http::header::CONTENT_LENGTH)
        && !req.body().is_empty()
    {
        let _ = write!(buf, "Content-Length: {}\r\n", req.body().len());
    }

    buf.push_str("\r\n");

    let mut bytes = buf.into_bytes();
    if !req.body().is_empty() {
        bytes.extend_from_slice(req.body());
    }
    bytes
}

/// Encode an HTTP/1.1 response into wire bytes.
///
/// Format: `HTTP/1.1 SP status SP reason CRLF headers CRLF body`
pub fn encode_response(resp: &http::Response<Vec<u8>>) -> Vec<u8> {
    let mut buf = String::with_capacity(256);
    let has_transfer_encoding = has_transfer_encoding(resp.headers());

    // Status line
    let reason = resp.status().canonical_reason().unwrap_or("Unknown");
    let _ = write!(buf, "HTTP/1.1 {} {}\r\n", resp.status().as_u16(), reason,);

    // Headers
    for (name, value) in resp.headers() {
        // RFC 9112 §6.1: a message with both Transfer-Encoding and Content-Length is
        // invalid (and a request-smuggling risk). Transfer-Encoding is authoritative:
        // suppress the auto-added Content-Length AND strip any caller-supplied one.
        if has_transfer_encoding && name == http::header::CONTENT_LENGTH {
            continue;
        }
        let _ = write!(
            buf,
            "{}: {}\r\n",
            name.as_str(),
            value.to_str().unwrap_or("")
        );
    }

    // Add Content-Length if not present
    if !has_transfer_encoding && !resp.headers().contains_key(http::header::CONTENT_LENGTH) {
        let _ = write!(buf, "Content-Length: {}\r\n", resp.body().len());
    }

    buf.push_str("\r\n");

    let mut bytes = buf.into_bytes();
    if !resp.body().is_empty() {
        bytes.extend_from_slice(resp.body());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_get_request() {
        let req = http::Request::builder()
            .method("GET")
            .uri("/index.html")
            .header("Host", "example.com")
            .body(Vec::new())
            .unwrap();

        let bytes = encode_request(&req);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.starts_with("GET /index.html HTTP/1.1\r\n"));
        assert!(s.contains("host: example.com\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn encode_post_with_body() {
        let body = b"hello world".to_vec();
        let req = http::Request::builder()
            .method("POST")
            .uri("/api")
            .body(body)
            .unwrap();

        let bytes = encode_request(&req);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("Content-Length: 11\r\n"));
        assert!(s.ends_with("hello world"));
    }

    #[test]
    fn encode_200_response() {
        let resp = http::Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(b"ok".to_vec())
            .unwrap();

        let bytes = encode_response(&resp);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("content-type: text/plain\r\n"));
        assert!(s.contains("Content-Length: 2\r\n"));
        assert!(s.ends_with("ok"));
    }

    #[test]
    fn encode_request_with_transfer_encoding_omits_content_length() {
        let req = http::Request::builder()
            .method("POST")
            .uri("/upload")
            .header("Transfer-Encoding", "chunked")
            .header("Content-Length", "4")
            .body(b"body".to_vec())
            .unwrap();

        let bytes = encode_request(&req);
        let s = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
        assert!(s.contains("transfer-encoding: chunked\r\n"));
        assert!(!s.contains("content-length:"));
    }

    #[test]
    fn encode_response_with_transfer_encoding_omits_content_length() {
        let resp = http::Response::builder()
            .status(200)
            .header("Transfer-Encoding", "chunked")
            .header("Content-Length", "2")
            .body(b"ok".to_vec())
            .unwrap();

        let bytes = encode_response(&resp);
        let s = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
        assert!(s.contains("transfer-encoding: chunked\r\n"));
        assert!(!s.contains("content-length:"));
    }
}
