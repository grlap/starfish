//! HTTP/1.1 client — sends requests and reads responses.
//!
//! [`H1Client`] wraps an [`H1Connection`] and provides a simple
//! `send()` method for request/response exchanges. Supports optional
//! read and write timeouts via the reactor's `IOTimeout` mechanism.

use std::time::Duration;

use starfish_reactor::cooperative_io::async_read::AsyncRead;
use starfish_reactor::cooperative_io::async_write::{AsyncWrite, AsyncWriteExtension};
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;

use super::connection::H1Connection;
use super::encoder;
use super::parser::ResponseParser;
use crate::content_coding;
use crate::error::HttpError;

/// HTTP/1.1 client over an existing connection.
pub struct H1Client<S> {
    conn: H1Connection<S>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl<S: AsyncRead + AsyncWrite> H1Client<S> {
    /// Create a new H1Client wrapping the given stream.
    pub fn new(stream: S) -> Self {
        Self {
            conn: H1Connection::new(stream),
            read_timeout: None,
            write_timeout: None,
        }
    }

    /// Create from an existing H1Connection.
    pub fn from_connection(conn: H1Connection<S>) -> Self {
        Self {
            conn,
            read_timeout: None,
            write_timeout: None,
        }
    }

    /// Set the read timeout for response reads.
    ///
    /// Each individual read call will time out after this duration.
    /// `None` disables the timeout (reads can block indefinitely).
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    /// Set the write timeout for request writes.
    ///
    /// The entire request write must complete within this duration.
    /// `None` disables the timeout.
    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    /// Send an HTTP request and read the response.
    ///
    /// Automatically handles informational (1xx) responses: any 100 Continue
    /// or other 1xx response is consumed and discarded, then the parser is
    /// reset to read the final response. Unconsumed bytes from the same read
    /// buffer are re-fed to the new parser.
    pub async fn send(
        &mut self,
        mut request: http::Request<Vec<u8>>,
    ) -> Result<http::Response<Vec<u8>>, HttpError> {
        let request_method = request.method().clone();
        content_coding::ensure_accept_encoding(request.headers_mut());

        // Encode and send the request (with write timeout)
        let write_to = self.write_timeout.map(IOTimeout::from_duration);
        let wire = encoder::encode_request(&request);
        self.conn
            .stream
            .write_all_with_timeout(&wire, &write_to)
            .await
            .map_err(Self::map_timeout_error)?;
        self.conn
            .stream
            .flush_with_timeout(&write_to)
            .await
            .map_err(Self::map_timeout_error)?;

        // Read the response, restarting on 1xx informational responses
        let mut parser = ResponseParser::new();
        let mut read_buf = [0u8; 4096];

        loop {
            let read_to = self.read_timeout.map(IOTimeout::from_duration);
            let n = self
                .conn
                .stream
                .read_with_timeout(&mut read_buf, &read_to)
                .await
                .map_err(Self::map_timeout_error)?;

            if n == 0 {
                parser.signal_eof();
                if parser.is_complete() {
                    break;
                }
                return Err(HttpError::UnexpectedEof);
            }

            let (consumed, mut complete) = parser.feed(&read_buf[..n])?;
            if request_method == http::Method::HEAD && parser.is_head_complete() {
                parser.complete_without_body();
                complete = true;
            }
            if complete {
                // Check if this is an informational (1xx) response
                let mut resp = parser.take_response()?;
                if resp.status().is_informational() {
                    // Discard the 1xx response and reset for the real response
                    parser = ResponseParser::new();
                    // Re-feed any unconsumed bytes from the same read buffer
                    if consumed < n {
                        let (_, mut done) = parser.feed(&read_buf[consumed..n])?;
                        if request_method == http::Method::HEAD && parser.is_head_complete() {
                            parser.complete_without_body();
                            done = true;
                        }
                        if done {
                            break;
                        }
                    }
                    continue;
                }
                // Final response — update connection state and return
                self.conn.update_keep_alive(parser.keep_alive());
                if let Some(max) = parser.keep_alive_max() {
                    self.conn.max_requests = Some(max);
                }
                self.conn.increment_request_count();
                self.conn.touch();
                content_coding::decode_response_body_for_request(&mut resp, Some(&request_method))?;
                return Ok(resp);
            }
        }

        // Update connection state
        self.conn.update_keep_alive(parser.keep_alive());
        if let Some(max) = parser.keep_alive_max() {
            self.conn.max_requests = Some(max);
        }
        self.conn.increment_request_count();
        self.conn.touch();

        let mut response = parser.take_response()?;
        content_coding::decode_response_body_for_request(&mut response, Some(&request_method))?;
        Ok(response)
    }

    /// Whether the underlying connection can be reused.
    pub fn is_reusable(&self) -> bool {
        self.conn.is_reusable()
    }

    /// Consume the client and return the underlying connection.
    pub fn into_connection(self) -> H1Connection<S> {
        self.conn
    }

    /// Map I/O errors, converting `TimedOut` to `HttpError::Timeout`.
    fn map_timeout_error(e: std::io::Error) -> HttpError {
        if e.kind() == std::io::ErrorKind::TimedOut {
            HttpError::Timeout
        } else {
            HttpError::Io(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::io;
    use std::io::Write;
    use std::rc::Rc;

    use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
    use starfish_reactor::cooperative_io::io_timeout::IOTimeout;

    #[derive(Default)]
    struct MockState {
        reads: VecDeque<io::Result<Vec<u8>>>,
        written: Vec<u8>,
        flushes: usize,
    }

    #[derive(Clone, Default)]
    struct MockStream {
        state: Rc<RefCell<MockState>>,
    }

    impl MockStream {
        fn with_reads(reads: Vec<io::Result<Vec<u8>>>) -> Self {
            Self {
                state: Rc::new(RefCell::new(MockState {
                    reads: VecDeque::from(reads),
                    written: Vec::new(),
                    flushes: 0,
                })),
            }
        }

        fn written_bytes(&self) -> Vec<u8> {
            self.state.borrow().written.clone()
        }

        fn flush_count(&self) -> usize {
            self.state.borrow().flushes
        }
    }

    impl AsyncRead for MockStream {
        fn read_with_timeout(
            &mut self,
            buf: &mut [u8],
            _timeout: &Option<IOTimeout>,
        ) -> impl Future<Output = io::Result<usize>> {
            let next = self
                .state
                .borrow_mut()
                .reads
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()));

            async move {
                let chunk = next?;
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                Ok(n)
            }
        }
    }

    impl AsyncWrite for MockStream {
        fn write_with_timeout(
            &mut self,
            buf: &[u8],
            _timeout: &Option<IOTimeout>,
        ) -> impl Future<Output = io::Result<usize>> {
            let state = self.state.clone();
            let data = buf.to_vec();
            async move {
                state.borrow_mut().written.extend_from_slice(&data);
                Ok(data.len())
            }
        }

        fn flush_with_timeout(
            &mut self,
            _timeout: &Option<IOTimeout>,
        ) -> impl Future<Output = io::Result<()>> {
            let state = self.state.clone();
            async move {
                state.borrow_mut().flushes += 1;
                Ok(())
            }
        }
    }

    #[test]
    fn send_writes_request_and_reads_response() {
        let stream = MockStream::with_reads(vec![Ok(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nKeep-Alive: max=2\r\n\r\nhello".to_vec(),
        )]);
        let mut client = H1Client::new(stream.clone());
        let request = http::Request::builder()
            .method("POST")
            .uri("/submit")
            .header("Host", "example.test")
            .body(b"payload".to_vec())
            .unwrap();

        let response = client.send(request).unwrap_result().unwrap();

        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.body().as_slice(), b"hello");
        assert!(client.is_reusable());

        let written = String::from_utf8(stream.written_bytes()).unwrap();
        assert!(written.starts_with("POST /submit HTTP/1.1\r\n"));
        assert!(written.contains("Content-Length: 7\r\n"));
        assert!(written.ends_with("payload"));
        assert_eq!(stream.flush_count(), 1);
    }

    #[test]
    fn send_maps_timed_out_reads_to_http_timeout() {
        let stream = MockStream::with_reads(vec![Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "read timed out",
        ))]);
        let mut client = H1Client::new(stream);
        let request = http::Request::builder()
            .method("GET")
            .uri("/slow")
            .body(Vec::new())
            .unwrap();

        let err = client.send(request).unwrap_result().unwrap_err();
        assert!(matches!(err, HttpError::Timeout));
    }

    #[test]
    fn send_adds_accept_encoding_and_decodes_gzip_response() {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"compressed").unwrap();
        let compressed = encoder.finish().unwrap();

        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            compressed.len()
        )
        .into_bytes();
        response.extend_from_slice(&compressed);

        let stream = MockStream::with_reads(vec![Ok(response)]);
        let mut client = H1Client::new(stream.clone());
        let request = http::Request::builder()
            .method("GET")
            .uri("/gzip")
            .header("Host", "example.test")
            .body(Vec::new())
            .unwrap();

        let response = client.send(request).unwrap_result().unwrap();

        assert_eq!(response.body().as_slice(), b"compressed");
        assert!(response
            .headers()
            .get(http::header::CONTENT_ENCODING)
            .is_none());

        let written = String::from_utf8(stream.written_bytes())
            .unwrap()
            .to_ascii_lowercase();
        assert!(written.contains("accept-encoding: gzip, br, zstd\r\n"));
    }

    #[test]
    fn head_response_with_content_encoding_skips_body_decode() {
        let stream = MockStream::with_reads(vec![Ok(
            b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 24\r\n\r\n".to_vec(),
        )]);
        let mut client = H1Client::new(stream);
        let request = http::Request::builder()
            .method(http::Method::HEAD)
            .uri("/head")
            .header("Host", "example.test")
            .body(Vec::new())
            .unwrap();

        let response = client.send(request).unwrap_result().unwrap();

        assert_eq!(response.status(), http::StatusCode::OK);
        assert!(response.body().is_empty());
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()),
            Some("24")
        );
    }
}
