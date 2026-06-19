//! Flow control (RFC 9000 §4).
//!
//! QUIC has two levels of flow control:
//! - **Connection-level**: limits total data across all streams
//! - **Stream-level**: limits data on each individual stream
//!
//! Credits are granted via MAX_DATA and MAX_STREAM_DATA frames.

/// Connection-level flow control state.
#[derive(Debug)]
pub struct ConnectionFlowControl {
    /// Maximum data the peer allows us to send (their MAX_DATA).
    send_max: u64,
    /// Total bytes we have sent across all streams.
    sent: u64,
    /// Maximum data we allow the peer to send (our MAX_DATA).
    recv_max: u64,
    /// Total bytes received across all streams.
    received: u64,
    /// The initial window size for auto-tuning.
    initial_window: u64,
}

impl ConnectionFlowControl {
    pub fn new(initial_send_max: u64, initial_recv_max: u64) -> Self {
        Self {
            send_max: initial_send_max,
            sent: 0,
            recv_max: initial_recv_max,
            received: 0,
            initial_window: initial_recv_max,
        }
    }

    /// How many bytes we can still send at the connection level.
    pub fn send_window(&self) -> u64 {
        self.send_max.saturating_sub(self.sent)
    }

    /// Record that we sent `n` bytes.
    pub fn on_data_sent(&mut self, n: u64) {
        self.sent += n;
    }

    /// Update the send limit (peer sent MAX_DATA).
    pub fn update_send_max(&mut self, new_max: u64) {
        if new_max > self.send_max {
            self.send_max = new_max;
        }
    }

    /// Set the send limit from peer transport parameters.
    pub fn set_send_max(&mut self, new_max: u64) {
        self.send_max = new_max;
    }

    /// Record that we received `n` new bytes from the peer at the connection
    /// level.  The caller is responsible for computing the delta (new bytes
    /// only) so that retransmissions are not double-counted.
    pub fn on_data_received(&mut self, n: u64) {
        self.received += n;
    }

    /// Check whether we should send a MAX_DATA update to the peer.
    /// Auto-extends when more than half the window has been consumed.
    pub fn should_send_max_data(&self) -> Option<u64> {
        let consumed = self.received;
        let threshold = self.recv_max.saturating_sub(self.initial_window / 2);
        if consumed >= threshold {
            Some(self.received + self.initial_window)
        } else {
            None
        }
    }

    /// Commit a MAX_DATA update that we're about to send.
    pub fn commit_max_data(&mut self, new_max: u64) {
        self.recv_max = new_max;
    }

    /// Use a DATA_BLOCKED hint to resend or proactively extend MAX_DATA.
    ///
    /// Security: the new limit grows from our own `recv_max` (by at most one local
    /// window), never from the peer-supplied `blocked_at`. Deriving it from peer input
    /// would let a malicious BLOCKED hint push our receive window toward `u64::MAX` and
    /// disable flow control (RFC 9000 §4.1).
    pub fn update_from_data_blocked(&mut self, blocked_at: u64) -> Option<u64> {
        if blocked_at < self.recv_max {
            return Some(self.recv_max);
        }

        let new_max = self.recv_max.saturating_add(self.initial_window);
        if new_max > self.recv_max {
            self.recv_max = new_max;
        }
        Some(self.recv_max)
    }

    /// Check if the peer has exceeded our flow control limit.
    pub fn check_recv_limit(&self) -> bool {
        self.received <= self.recv_max
    }
}

/// Per-stream flow control state.
#[derive(Debug)]
pub struct StreamFlowControl {
    /// Maximum data the peer allows us to send on this stream.
    send_max: u64,
    /// Bytes sent on this stream.
    sent: u64,
    /// Maximum data we allow the peer to send on this stream.
    recv_max: u64,
    /// Highest byte offset received on this stream (`offset + data.len()`).
    /// RFC 9000 §4.1: flow control is based on the maximum offset, not
    /// cumulative bytes, so retransmissions at overlapping offsets are not
    /// double-counted.
    received: u64,
    /// Initial window for auto-tuning.
    initial_window: u64,
}

impl StreamFlowControl {
    pub fn new(initial_send_max: u64, initial_recv_max: u64) -> Self {
        Self {
            send_max: initial_send_max,
            sent: 0,
            recv_max: initial_recv_max,
            received: 0,
            initial_window: initial_recv_max,
        }
    }

    /// How many bytes we can still send on this stream.
    pub fn send_window(&self) -> u64 {
        self.send_max.saturating_sub(self.sent)
    }

    /// Record bytes sent.
    pub fn on_data_sent(&mut self, n: u64) {
        self.sent += n;
    }

    /// Update the send limit (peer sent MAX_STREAM_DATA).
    pub fn update_send_max(&mut self, new_max: u64) {
        if new_max > self.send_max {
            self.send_max = new_max;
        }
    }

    /// Set the send limit from peer transport parameters.
    pub fn set_send_max(&mut self, new_max: u64) {
        self.send_max = new_max;
    }

    /// Update the highest byte offset received on this stream.
    ///
    /// Returns the number of *new* bytes (the delta) that the connection-level
    /// flow control should be charged.  Overlapping/retransmitted ranges
    /// return 0 so they are not double-counted (RFC 9000 §4.1).
    pub fn on_data_received(&mut self, offset: u64, len: u64) -> u64 {
        let end = offset + len;
        if end > self.received {
            let delta = end - self.received;
            self.received = end;
            delta
        } else {
            0
        }
    }

    /// Check whether we should send MAX_STREAM_DATA.
    pub fn should_send_max_stream_data(&self) -> Option<u64> {
        let threshold = self.recv_max.saturating_sub(self.initial_window / 2);
        if self.received >= threshold {
            Some(self.received + self.initial_window)
        } else {
            None
        }
    }

    /// Commit a MAX_STREAM_DATA update.
    pub fn commit_max_stream_data(&mut self, new_max: u64) {
        self.recv_max = new_max;
    }

    /// Use a STREAM_DATA_BLOCKED hint to resend or proactively extend MAX_STREAM_DATA.
    ///
    /// Security: the new limit grows from our own `recv_max` (by at most one local
    /// window), never from the peer-supplied `blocked_at`. Deriving it from peer input
    /// would let a malicious BLOCKED hint push our receive window toward `u64::MAX` and
    /// disable flow control (RFC 9000 §4.1).
    pub fn update_from_stream_data_blocked(&mut self, blocked_at: u64) -> Option<u64> {
        if blocked_at < self.recv_max {
            return Some(self.recv_max);
        }

        let new_max = self.recv_max.saturating_add(self.initial_window);
        if new_max > self.recv_max {
            self.recv_max = new_max;
        }
        Some(self.recv_max)
    }

    /// Check if the peer has exceeded this stream's flow control limit.
    pub fn check_recv_limit(&self) -> bool {
        self.received <= self.recv_max
    }

    /// Highest byte offset received on the stream.
    pub fn received_max_offset(&self) -> u64 {
        self.received
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_flow_control_basic() {
        let mut fc = ConnectionFlowControl::new(1000, 1000);
        assert_eq!(fc.send_window(), 1000);

        fc.on_data_sent(300);
        assert_eq!(fc.send_window(), 700);

        fc.update_send_max(2000);
        assert_eq!(fc.send_window(), 1700);
    }

    #[test]
    fn connection_flow_control_recv_limit() {
        let mut fc = ConnectionFlowControl::new(1000, 1000);
        fc.on_data_received(500);
        assert!(fc.check_recv_limit());

        fc.on_data_received(501);
        assert!(!fc.check_recv_limit());
    }

    #[test]
    fn connection_flow_control_auto_extend() {
        let mut fc = ConnectionFlowControl::new(1000, 1000);
        // Threshold = 1000 - 500 = 500
        fc.on_data_received(499);
        assert!(fc.should_send_max_data().is_none());

        fc.on_data_received(1);
        let new_max = fc.should_send_max_data().unwrap();
        assert_eq!(new_max, 500 + 1000); // received + initial_window
        fc.commit_max_data(new_max);
    }

    #[test]
    fn data_blocked_only_extends_from_current_limit() {
        let mut fc = ConnectionFlowControl::new(1000, 1024);

        let new_max = fc.update_from_data_blocked(u64::MAX).unwrap();

        assert_eq!(new_max, 2048);
        assert!(fc.check_recv_limit());
    }

    #[test]
    fn stream_flow_control_basic() {
        let mut fc = StreamFlowControl::new(500, 500);
        assert_eq!(fc.send_window(), 500);

        fc.on_data_sent(200);
        assert_eq!(fc.send_window(), 300);

        fc.update_send_max(800);
        assert_eq!(fc.send_window(), 600);
    }

    #[test]
    fn stream_flow_control_auto_extend() {
        let mut fc = StreamFlowControl::new(1000, 1000);
        fc.on_data_received(0, 500);
        let new_max = fc.should_send_max_stream_data().unwrap();
        assert_eq!(new_max, 1500);
        fc.commit_max_stream_data(new_max);
    }

    #[test]
    fn stream_data_blocked_only_extends_from_current_limit() {
        let mut fc = StreamFlowControl::new(1000, 2048);

        let new_max = fc.update_from_stream_data_blocked(u64::MAX).unwrap();

        assert_eq!(new_max, 4096);
        assert!(fc.check_recv_limit());
    }

    #[test]
    fn stream_flow_control_dedup_retransmission() {
        let mut fc = StreamFlowControl::new(1000, 1000);

        // First receive: offset 0, 100 bytes → max offset = 100
        let delta = fc.on_data_received(0, 100);
        assert_eq!(delta, 100);

        // Retransmission of the same range — delta should be 0
        let delta = fc.on_data_received(0, 100);
        assert_eq!(delta, 0);

        // Overlapping range: offset 50, 100 bytes → end = 150, new bytes = 50
        let delta = fc.on_data_received(50, 100);
        assert_eq!(delta, 50);

        assert!(fc.check_recv_limit()); // 150 <= 1000
    }
}
