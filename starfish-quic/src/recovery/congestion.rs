//! Congestion controller trait (RFC 9002 §7).
//!
//! Defines the interface that NewReno and Cubic implement.

use std::time::{Duration, Instant};

/// The minimum congestion window in bytes (RFC 9002 §7.2).
pub const MINIMUM_WINDOW: usize = 2 * 1200; // 2 * max datagram size

/// The initial congestion window in bytes (RFC 9002 §7.2).
pub const INITIAL_WINDOW: usize = 10 * 1200; // 10 * max datagram size

/// Congestion controller interface.
pub trait CongestionController: std::fmt::Debug {
    /// Create a fresh instance of the same algorithm with initial state.
    fn new_instance(&self) -> Box<dyn CongestionController>;

    /// Called when a packet containing `bytes` is sent.
    fn on_packet_sent(&mut self, bytes: usize, now: Instant);

    /// Called when `bytes` are newly acknowledged.
    /// `sent_time` is when the acknowledged packet was sent (for recovery check).
    /// `now` is the current time (ACK receipt) for congestion epoch timing.
    fn on_ack(&mut self, bytes: usize, rtt: Duration, sent_time: Instant, now: Instant);

    /// Called when packets containing `bytes` are detected as lost.
    /// `sent_time` is when the lost packet was originally sent (for recovery period check).
    fn on_loss(&mut self, bytes: usize, sent_time: Instant, now: Instant);

    /// Called when an acknowledged ECN-capable packet is reported as CE-marked.
    /// `sent_time` is when the packet was sent and `now` is when the ACK arrived.
    fn on_ecn_ce(&mut self, bytes: usize, sent_time: Instant, now: Instant) {
        self.on_loss(bytes, sent_time, now);
    }

    /// Called when tracked in-flight bytes are discarded without counting as loss.
    fn on_packet_discarded(&mut self, bytes: usize);

    /// Called when the persistent congestion threshold is met.
    fn on_persistent_congestion(&mut self);

    /// The current congestion window in bytes.
    fn window(&self) -> usize;

    /// Bytes currently in flight (sent but not yet acknowledged or lost).
    fn bytes_in_flight(&self) -> usize;

    /// Whether we can send `bytes` more data.
    fn can_send(&self, bytes: usize) -> bool {
        self.bytes_in_flight() + bytes <= self.window()
    }

    /// The current slow start threshold (test/diagnostic only).
    #[allow(dead_code)]
    fn ssthresh(&self) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::{cubic::Cubic, new_reno::NewReno};
    use std::time::Instant;

    // Verify constants match RFC 9002 recommendations
    #[test]
    fn constants() {
        assert_eq!(MINIMUM_WINDOW, 2400);
        assert_eq!(INITIAL_WINDOW, 12000);
    }

    #[test]
    fn trait_object_dispatches_core_signals() {
        let now = Instant::now();
        let rtt = Duration::from_millis(10);

        let mut controllers: Vec<Box<dyn CongestionController>> =
            vec![Box::new(NewReno::new()), Box::new(Cubic::new())];

        for controller in &mut controllers {
            controller.on_packet_sent(1200, now);
            assert_eq!(controller.bytes_in_flight(), 1200);

            controller.on_ack(1200, rtt, now, now + rtt);
            assert_eq!(controller.bytes_in_flight(), 0);

            controller.on_packet_sent(1200, now + rtt);
            controller.on_ecn_ce(1200, now + rtt, now + rtt * 2);
            assert_eq!(controller.bytes_in_flight(), 0);

            controller.on_packet_sent(1200, now + rtt * 2);
            controller.on_loss(1200, now + rtt * 2, now + rtt * 3);
            assert_eq!(controller.bytes_in_flight(), 0);
        }
    }
}
