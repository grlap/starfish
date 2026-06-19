//! HTTP/1.1 server — reads requests and sends responses.
//!
//! [`H1Server`] wraps an [`H1Connection`] and provides methods for
//! reading incoming requests and sending responses on a per-connection basis.

use starfish_reactor::cooperative_io::async_read::{AsyncRead, AsyncReadExtension};
use starfish_reactor::cooperative_io::async_write::{AsyncWrite, AsyncWriteExtension};

use super::connection::H1Connection;
use super::encoder;
use super::parser::RequestParser;
use crate::error::HttpError;

/// HTTP/1.1 server connection handler.
pub struct H1Server<S> {
    conn: H1Connection<S>,
}

impl<S: AsyncRead + AsyncWrite> H1Server<S> {
    /// Create a new H1Server wrapping the given stream.
    pub fn new(stream: S) -> Self {
        Self {
            conn: H1Connection::new(stream),
        }
    }

    /// Read the next HTTP request from the connection.
    ///
    /// Returns `None` if the connection was closed cleanly.
    pub async fn recv_request(&mut self) -> Result<Option<http::Request<Vec<u8>>>, HttpError> {
        let mut parser = RequestParser::new();
        let mut read_buf = [0u8; 4096];
        let mut received_any = false;

        loop {
            let n = self
                .conn
                .stream
                .read(&mut read_buf)
                .await
                .map_err(HttpError::Io)?;

            if n == 0 {
                if parser.is_complete() {
                    return Ok(Some(parser.take_request()?));
                }
                // Clean close before any data → no request pending
                if !received_any {
                    return Ok(None);
                }
                // Closed mid-parse → unexpected EOF
                return Err(HttpError::UnexpectedEof);
            }
            received_any = true;

            let (_, complete) = parser.feed(&read_buf[..n])?;
            if complete {
                break;
            }
        }

        self.conn.update_keep_alive(parser.keep_alive());
        self.conn.increment_request_count();

        Ok(Some(parser.take_request()?))
    }

    /// Send an HTTP response.
    pub async fn send_response(
        &mut self,
        response: http::Response<Vec<u8>>,
    ) -> Result<(), HttpError> {
        let wire = encoder::encode_response(&response);
        self.conn
            .stream
            .write_all(&wire)
            .await
            .map_err(HttpError::Io)?;
        self.conn.stream.flush().await.map_err(HttpError::Io)?;
        Ok(())
    }

    /// Whether the connection should accept more requests.
    pub fn is_reusable(&self) -> bool {
        self.conn.is_reusable()
    }

    /// Consume the server and return the underlying connection.
    pub fn into_connection(self) -> H1Connection<S> {
        self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::io;
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
    fn recv_request_parses_request_and_reports_clean_close() {
        let stream = MockStream::with_reads(vec![
            Ok(
                b"POST /submit HTTP/1.1\r\nHost: example.test\r\nContent-Length: 5\r\n\r\nhello"
                    .to_vec(),
            ),
            Ok(Vec::new()),
        ]);
        let mut server = H1Server::new(stream);

        let request = server.recv_request().unwrap_result().unwrap().unwrap();
        assert_eq!(request.method(), http::Method::POST);
        assert_eq!(request.uri().path(), "/submit");
        assert_eq!(request.body().as_slice(), b"hello");
        assert!(server.is_reusable());

        let no_request = server.recv_request().unwrap_result().unwrap();
        assert!(no_request.is_none());
    }

    #[test]
    fn send_response_writes_wire_response() {
        let stream = MockStream::default();
        let mut server = H1Server::new(stream.clone());
        let response = http::Response::builder()
            .status(201)
            .header("Content-Type", "text/plain")
            .body(b"created".to_vec())
            .unwrap();

        server.send_response(response).unwrap_result().unwrap();

        let written = String::from_utf8(stream.written_bytes()).unwrap();
        assert!(written.starts_with("HTTP/1.1 201 Created\r\n"));
        assert!(written.contains("content-type: text/plain\r\n"));
        assert!(written.contains("Content-Length: 7\r\n"));
        assert!(written.ends_with("created"));
        assert_eq!(stream.flush_count(), 1);
    }
}
