//! NewReno congestion control (RFC 9002 §7).
//!
//! A simple loss-based congestion control algorithm with slow start and
//! congestion avoidance phases.

use super::congestion::{CongestionController, INITIAL_WINDOW, MINIMUM_WINDOW};
use std::time::{Duration, Instant};

/// NewReno congestion controller.
#[derive(Debug)]
#[allow(dead_code)]
pub struct NewReno {
    /// Current congestion window in bytes.
    cwnd: usize,
    /// Slow start threshold.
    ssthresh: usize,
    /// Bytes in flight.
    bytes_in_flight: usize,
    /// Time when the most recent congestion event was detected.
    congestion_recovery_start: Option<Instant>,
}

impl Default for NewReno {
    fn default() -> Self {
        Self::new()
    }
}

impl NewReno {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            cwnd: INITIAL_WINDOW,
            ssthresh: usize::MAX,
            bytes_in_flight: 0,
            congestion_recovery_start: None,
        }
    }

    #[allow(dead_code)]
    fn in_congestion_recovery(&self, sent_time: Instant) -> bool {
        match self.congestion_recovery_start {
            Some(start) => sent_time <= start,
            None => false,
        }
    }
}

impl CongestionController for NewReno {
    fn new_instance(&self) -> Box<dyn CongestionController> {
        Box::new(NewReno::new())
    }

    fn on_packet_sent(&mut self, bytes: usize, _now: Instant) {
        self.bytes_in_flight += bytes;
    }

    fn on_ack(&mut self, bytes: usize, _rtt: Duration, sent_time: Instant, _now: Instant) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);

        if self.in_congestion_recovery(sent_time) {
            // Don't increase cwnd during recovery
            return;
        }

        if self.cwnd < self.ssthresh {
            // Slow start: increase by the number of bytes acknowledged
            self.cwnd += bytes;
        } else {
            // Congestion avoidance: increase by ~1 MSS per RTT
            // cwnd += MSS * bytes / cwnd
            self.cwnd += (1200 * bytes) / self.cwnd;
        }
    }

    fn on_loss(&mut self, bytes: usize, sent_time: Instant, now: Instant) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);

        // Only reduce once per recovery period
        if !self.in_congestion_recovery(sent_time) {
            self.congestion_recovery_start = Some(now);
            self.ssthresh = self.cwnd / 2;
            self.cwnd = self.ssthresh.max(MINIMUM_WINDOW);
        }
    }

    fn on_ecn_ce(&mut self, bytes: usize, sent_time: Instant, now: Instant) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);

        if !self.in_congestion_recovery(sent_time) {
            self.congestion_recovery_start = Some(now);
            self.ssthresh = self.cwnd / 2;
            self.cwnd = self.ssthresh.max(MINIMUM_WINDOW);
        }
    }

    fn on_packet_discarded(&mut self, bytes: usize) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);
    }

    fn on_persistent_congestion(&mut self) {
        self.cwnd = MINIMUM_WINDOW;
        self.ssthresh = usize::MAX;
    }

    fn window(&self) -> usize {
        self.cwnd
    }

    fn bytes_in_flight(&self) -> usize {
        self.bytes_in_flight
    }

    fn ssthresh(&self) -> usize {
        self.ssthresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let cc = NewReno::new();
        assert_eq!(cc.window(), INITIAL_WINDOW);
        assert_eq!(cc.bytes_in_flight(), 0);
        assert_eq!(cc.ssthresh(), usize::MAX);
    }

    #[test]
    fn slow_start_growth() {
        let mut cc = NewReno::new();
        let now = Instant::now();
        let rtt = Duration::from_millis(100);

        cc.on_packet_sent(1200, now);
        assert_eq!(cc.bytes_in_flight(), 1200);

        cc.on_ack(1200, rtt, now + rtt, now + rtt);
        assert_eq!(cc.bytes_in_flight(), 0);
        // cwnd should have grown by 1200 (slow start)
        assert_eq!(cc.window(), INITIAL_WINDOW + 1200);
    }

    #[test]
    fn loss_halves_window() {
        let mut cc = NewReno::new();
        let now = Instant::now();

        cc.on_packet_sent(6000, now);
        cc.on_loss(1200, now, now + Duration::from_millis(1));

        assert_eq!(cc.ssthresh(), INITIAL_WINDOW / 2);
        assert_eq!(cc.window(), INITIAL_WINDOW / 2);
    }

    #[test]
    fn persistent_congestion_resets() {
        let mut cc = NewReno::new();
        cc.on_persistent_congestion();
        assert_eq!(cc.window(), MINIMUM_WINDOW);
        assert_eq!(cc.ssthresh(), usize::MAX);
    }

    #[test]
    fn persistent_congestion_restarts_slow_start() {
        let mut cc = NewReno::new();
        let now = Instant::now();
        let rtt = Duration::from_millis(50);

        cc.on_persistent_congestion();
        cc.on_packet_sent(1200, now);
        cc.on_ack(1200, rtt, now, now + rtt);

        assert_eq!(cc.window(), MINIMUM_WINDOW + 1200);
    }

    #[test]
    fn congestion_avoidance() {
        let mut cc = NewReno::new();
        let now = Instant::now();
        let rtt = Duration::from_millis(100);

        // Force into congestion avoidance by lowering ssthresh
        cc.on_packet_sent(6000, now);
        cc.on_loss(1200, now, now + Duration::from_millis(1));
        let window_after_loss = cc.window();

        // Now acks should grow slowly (congestion avoidance)
        cc.on_packet_sent(1200, now + Duration::from_millis(2));
        cc.on_ack(
            1200,
            rtt,
            now + Duration::from_millis(2),
            now + Duration::from_millis(102),
        );

        // Growth should be much smaller than in slow start
        let growth = cc.window() - window_after_loss;
        assert!(
            growth < 1200,
            "congestion avoidance should grow slowly, grew by {growth}"
        );
    }

    #[test]
    fn on_loss_uses_sent_time_for_recovery() {
        let mut cc = NewReno::new();
        let now = Instant::now();

        cc.on_packet_sent(6000, now);

        // First loss: packet sent at t=0, detected at t=1ms
        cc.on_loss(1200, now, now + Duration::from_millis(1));
        let window_after_first = cc.window();

        // Second loss for a packet also sent at t=0 (within recovery period).
        // Should NOT reduce the window again since sent_time <= recovery start.
        cc.on_loss(1200, now, now + Duration::from_millis(2));
        assert_eq!(
            cc.window(),
            window_after_first,
            "loss within recovery period should not reduce cwnd"
        );
    }

    #[test]
    fn ecn_ce_halves_window() {
        let mut cc = NewReno::new();
        let now = Instant::now();

        cc.on_packet_sent(6000, now);
        cc.on_ecn_ce(1200, now, now + Duration::from_millis(1));

        assert_eq!(cc.ssthresh(), INITIAL_WINDOW / 2);
        assert_eq!(cc.window(), INITIAL_WINDOW / 2);
    }
}
