//! HTTP/1.1 connection with keep-alive management.
//!
//! [`H1Connection`] wraps any `AsyncRead + AsyncWrite` stream and tracks
//! keep-alive state across multiple request/response exchanges. Tracks
//! last activity time for idle connection health checks.

use std::time::{Duration, Instant};

use starfish_reactor::cooperative_io::async_read::AsyncRead;
use starfish_reactor::cooperative_io::async_write::AsyncWrite;

/// An HTTP/1.1 connection wrapping an underlying stream.
pub struct H1Connection<S> {
    /// The underlying transport stream.
    pub(crate) stream: S,
    /// Whether keep-alive is enabled for this connection.
    pub(crate) keep_alive: bool,
    /// Number of request/response exchanges completed on this connection.
    pub(crate) request_count: u64,
    /// Maximum number of requests allowed (from Keep-Alive header), if any.
    pub(crate) max_requests: Option<u64>,
    /// When this connection last completed a request/response exchange.
    pub(crate) last_activity: Instant,
}

impl<S: AsyncRead + AsyncWrite> H1Connection<S> {
    /// Create a new H1Connection wrapping the given stream.
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            keep_alive: true,
            request_count: 0,
            max_requests: None,
            last_activity: Instant::now(),
        }
    }

    /// Whether this connection can be reused for another request.
    pub fn is_reusable(&self) -> bool {
        if !self.keep_alive {
            return false;
        }
        if let Some(max) = self.max_requests {
            if self.request_count >= max {
                return false;
            }
        }
        true
    }

    /// Whether the connection is likely still alive based on idle time.
    ///
    /// Returns `false` if the connection has been idle longer than `idle_timeout`,
    /// indicating the server may have closed it.
    pub fn is_likely_healthy(&self, idle_timeout: Duration) -> bool {
        self.last_activity.elapsed() < idle_timeout
    }

    /// Update the last activity timestamp to now.
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Update keep-alive state from response headers.
    pub fn update_keep_alive(&mut self, keep_alive: bool) {
        self.keep_alive = keep_alive;
    }

    /// Increment the request counter.
    pub fn increment_request_count(&mut self) {
        self.request_count += 1;
    }

    /// Get a mutable reference to the underlying stream.
    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// Consume the connection and return the underlying stream.
    pub fn into_stream(self) -> S {
        self.stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use starfish_reactor::cooperative_io::io_timeout::IOTimeout;

    /// Minimal mock stream for testing H1Connection (not async-dependent).
    struct MockStream;
    impl AsyncRead for MockStream {
        async fn read_with_timeout(
            &mut self,
            _buf: &mut [u8],
            _timeout: &Option<IOTimeout>,
        ) -> std::io::Result<usize> {
            Ok(0)
        }
    }
    impl AsyncWrite for MockStream {
        async fn write_with_timeout(
            &mut self,
            buf: &[u8],
            _timeout: &Option<IOTimeout>,
        ) -> std::io::Result<usize> {
            Ok(buf.len())
        }
    }

    #[test]
    fn new_connection_is_reusable() {
        let conn = H1Connection::new(MockStream);
        assert!(conn.is_reusable());
    }

    #[test]
    fn not_reusable_after_keep_alive_disabled() {
        let mut conn = H1Connection::new(MockStream);
        conn.update_keep_alive(false);
        assert!(!conn.is_reusable());
    }

    #[test]
    fn reusable_under_max_requests() {
        let mut conn = H1Connection::new(MockStream);
        conn.max_requests = Some(3);
        conn.increment_request_count();
        conn.increment_request_count();
        assert!(conn.is_reusable()); // 2 < 3
    }

    #[test]
    fn not_reusable_at_max_requests() {
        let mut conn = H1Connection::new(MockStream);
        conn.max_requests = Some(2);
        conn.increment_request_count();
        conn.increment_request_count();
        assert!(!conn.is_reusable()); // 2 >= 2
    }

    #[test]
    fn not_reusable_over_max_requests() {
        let mut conn = H1Connection::new(MockStream);
        conn.max_requests = Some(1);
        conn.increment_request_count();
        conn.increment_request_count();
        assert!(!conn.is_reusable()); // 2 >= 1
    }

    #[test]
    fn reusable_no_max_with_many_requests() {
        let mut conn = H1Connection::new(MockStream);
        for _ in 0..100 {
            conn.increment_request_count();
        }
        assert!(conn.is_reusable()); // no max_requests set
    }

    #[test]
    fn into_stream_returns_stream() {
        let conn = H1Connection::new(MockStream);
        let _stream: MockStream = conn.into_stream();
    }

    #[test]
    fn is_likely_healthy_true_for_recent() {
        let conn = H1Connection::new(MockStream);
        assert!(conn.is_likely_healthy(Duration::from_secs(60)));
    }

    #[test]
    fn is_likely_healthy_false_for_expired() {
        let mut conn = H1Connection::new(MockStream);
        conn.last_activity = Instant::now() - Duration::from_secs(120);
        assert!(!conn.is_likely_healthy(Duration::from_secs(60)));
    }

    #[test]
    fn touch_resets_last_activity() {
        let mut conn = H1Connection::new(MockStream);
        conn.last_activity = Instant::now() - Duration::from_secs(120);
        assert!(!conn.is_likely_healthy(Duration::from_secs(60)));
        conn.touch();
        assert!(conn.is_likely_healthy(Duration::from_secs(60)));
    }
}
