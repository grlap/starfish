//! Cubic congestion control (RFC 8312 + RFC 9002).
//!
//! Cubic uses a cubic function of time since the last congestion event
//! to determine the congestion window. It provides better bandwidth
//! utilization than NewReno on high-BDP networks.

use super::congestion::{CongestionController, INITIAL_WINDOW, MINIMUM_WINDOW};
use std::time::{Duration, Instant};

/// Cubic scaling factor (RFC 8312 §4.1).
const CUBIC_C: f64 = 0.4;

/// Multiplicative decrease factor (RFC 8312 §4.5).
const CUBIC_BETA: f64 = 0.7;

/// Cubic congestion controller.
#[derive(Debug)]
pub struct Cubic {
    /// Current congestion window in bytes.
    cwnd: usize,
    /// Slow start threshold.
    ssthresh: usize,
    /// Bytes in flight.
    bytes_in_flight: usize,
    /// W_max: window size before the last congestion event, in **MSS units**
    /// (NOT bytes — `cwnd`/`ssthresh`/`bytes_in_flight` are in bytes). Convert with
    /// `cwnd / mss` before comparing to `w_max`, and multiply `w_cubic()` by `mss`
    /// to get bytes. Mixing the two units silently breaks the controller.
    w_max: f64,
    /// K: time period to reach W_max after a congestion event.
    k: f64,
    /// Start time of the current congestion epoch.
    epoch_start: Option<Instant>,
    /// Time of the last congestion event.
    congestion_recovery_start: Option<Instant>,
    /// MSS in bytes.
    mss: usize,
}

impl Default for Cubic {
    fn default() -> Self {
        Self::new()
    }
}

impl Cubic {
    pub fn new() -> Self {
        Self {
            cwnd: INITIAL_WINDOW,
            ssthresh: usize::MAX,
            bytes_in_flight: 0,
            w_max: 0.0,
            k: 0.0,
            epoch_start: None,
            congestion_recovery_start: None,
            mss: 1200,
        }
    }

    fn in_congestion_recovery(&self, sent_time: Instant) -> bool {
        match self.congestion_recovery_start {
            Some(start) => sent_time <= start,
            None => false,
        }
    }

    /// Apply the multiplicative decrease (shared by on_loss and on_ecn_ce).
    fn enter_congestion_recovery(&mut self, sent_time: Instant, now: Instant) {
        if self.in_congestion_recovery(sent_time) {
            return;
        }

        self.congestion_recovery_start = Some(now);
        self.epoch_start = None;

        let cwnd_mss = self.cwnd as f64 / self.mss as f64;

        // If we're still probing (cwnd < w_max from prior), use fast convergence.
        if cwnd_mss < self.w_max {
            self.w_max = cwnd_mss * (1.0 + CUBIC_BETA) / 2.0;
        } else {
            self.w_max = cwnd_mss;
        }

        self.k = Self::compute_k(self.w_max);
        self.ssthresh = (self.cwnd as f64 * CUBIC_BETA) as usize;
        self.cwnd = self.ssthresh.max(MINIMUM_WINDOW);
    }

    /// Compute the Cubic window W_cubic(t) (RFC 8312 §4.1).
    ///
    /// W_cubic(t) = C * (t - K)^3 + W_max
    fn w_cubic(&self, t: f64) -> f64 {
        CUBIC_C * (t - self.k).powi(3) + self.w_max
    }

    /// Compute K = cubic_root(W_max * (1 - beta) / C)
    fn compute_k(w_max: f64) -> f64 {
        ((w_max * (1.0 - CUBIC_BETA)) / CUBIC_C).cbrt()
    }
}

impl CongestionController for Cubic {
    fn new_instance(&self) -> Box<dyn CongestionController> {
        Box::new(Cubic::new())
    }

    fn on_packet_sent(&mut self, bytes: usize, _now: Instant) {
        self.bytes_in_flight += bytes;
    }

    fn on_ack(&mut self, bytes: usize, _rtt: Duration, sent_time: Instant, now: Instant) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);

        if self.in_congestion_recovery(sent_time) {
            return;
        }

        if self.cwnd < self.ssthresh {
            // Slow start
            self.cwnd += bytes;
            return;
        }

        // Congestion avoidance — Cubic function (RFC 8312 §4.1)
        // Use ACK receipt time (now) for epoch timing, not packet sent_time.
        let epoch_start = self.epoch_start.get_or_insert(now);
        let t = now.duration_since(*epoch_start).as_secs_f64();

        let w_cubic = self.w_cubic(t);
        let w_cubic_bytes = (w_cubic * self.mss as f64).max(0.0) as usize;

        if w_cubic_bytes > self.cwnd {
            // Cubic is above current window — grow towards it
            let increment = ((w_cubic_bytes - self.cwnd) * bytes) / self.cwnd;
            self.cwnd += increment.max(1);
        } else {
            // TCP-friendly region: grow like standard Reno
            self.cwnd += (self.mss * bytes) / self.cwnd;
        }
    }

    fn on_loss(&mut self, bytes: usize, sent_time: Instant, now: Instant) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);
        self.enter_congestion_recovery(sent_time, now);
    }

    fn on_ecn_ce(&mut self, bytes: usize, sent_time: Instant, now: Instant) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);
        self.enter_congestion_recovery(sent_time, now);
    }

    fn on_packet_discarded(&mut self, bytes: usize) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);
    }

    fn on_persistent_congestion(&mut self) {
        self.cwnd = MINIMUM_WINDOW;
        self.ssthresh = MINIMUM_WINDOW;
        self.epoch_start = None;
        self.w_max = 0.0;
        self.k = 0.0;
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
        let cc = Cubic::new();
        assert_eq!(cc.window(), INITIAL_WINDOW);
        assert_eq!(cc.bytes_in_flight(), 0);
    }

    #[test]
    fn slow_start_growth() {
        let mut cc = Cubic::new();
        let now = Instant::now();
        let rtt = Duration::from_millis(100);

        cc.on_packet_sent(1200, now);
        let ack_time = now + rtt;
        cc.on_ack(1200, rtt, now, ack_time);
        assert_eq!(cc.window(), INITIAL_WINDOW + 1200);
    }

    #[test]
    fn loss_reduces_window() {
        let mut cc = Cubic::new();
        let now = Instant::now();

        cc.on_packet_sent(6000, now);
        cc.on_loss(1200, now, now + Duration::from_millis(1));

        assert!(cc.window() < INITIAL_WINDOW);
        assert_eq!(cc.window(), (INITIAL_WINDOW as f64 * CUBIC_BETA) as usize);
    }

    #[test]
    fn cubic_recovery_growth() {
        let mut cc = Cubic::new();
        let now = Instant::now();
        let rtt = Duration::from_millis(50);

        // Get to a reasonable window
        for i in 0..20 {
            let sent_time = now + Duration::from_millis(i * 50);
            cc.on_packet_sent(1200, sent_time);
            let ack_time = sent_time + rtt;
            cc.on_ack(1200, rtt, sent_time, ack_time);
        }
        let peak = cc.window();

        // Trigger loss
        let loss_time = now + Duration::from_secs(1);
        cc.on_packet_sent(1200, loss_time);
        cc.on_loss(1200, loss_time, loss_time + Duration::from_millis(1));
        let window_after_loss = cc.window();
        assert!(window_after_loss < peak);

        // Recovery: window should grow back
        for i in 0..50 {
            let sent_time = loss_time + Duration::from_millis(100 + i * 50);
            cc.on_packet_sent(1200, sent_time);
            let ack_time = sent_time + rtt;
            cc.on_ack(1200, rtt, sent_time, ack_time);
        }
        assert!(cc.window() > window_after_loss);
    }

    #[test]
    fn cubic_converges_across_multiple_loss_recovery_cycles() {
        let mut cc = Cubic::new();
        let now = Instant::now();
        let rtt = Duration::from_millis(50);

        for i in 0..24 {
            let sent_time = now + Duration::from_millis(i * 50);
            cc.on_packet_sent(1200, sent_time);
            cc.on_ack(1200, rtt, sent_time, sent_time + rtt);
        }
        let first_peak = cc.window();

        let first_loss_sent = now + Duration::from_secs(2);
        cc.on_packet_sent(1200, first_loss_sent);
        cc.on_loss(
            1200,
            first_loss_sent,
            first_loss_sent + Duration::from_millis(1),
        );
        let first_valley = cc.window();
        assert!(first_valley < first_peak);

        for i in 0..32 {
            let sent_time = first_loss_sent + Duration::from_millis(100 + i * 50);
            cc.on_packet_sent(1200, sent_time);
            cc.on_ack(1200, rtt, sent_time, sent_time + rtt);
        }
        let second_peak = cc.window();
        assert!(second_peak > first_valley);

        let second_loss_sent = first_loss_sent + Duration::from_secs(3);
        cc.on_packet_sent(1200, second_loss_sent);
        cc.on_loss(
            1200,
            second_loss_sent,
            second_loss_sent + Duration::from_millis(1),
        );
        let second_valley = cc.window();
        assert!(second_valley < second_peak);

        for i in 0..100 {
            let sent_time = second_loss_sent + Duration::from_millis(100 + i * 50);
            cc.on_packet_sent(1200, sent_time);
            cc.on_ack(1200, rtt, sent_time, sent_time + rtt);
        }

        assert!(cc.window() > second_valley);
        assert!(cc.window() > first_valley);
        assert!(cc.window() > second_valley + 4 * 1200);
    }

    #[test]
    fn fast_convergence_compares_window_in_mss_units() {
        let mut cc = Cubic::new();
        let sent = Instant::now();
        cc.cwnd = 12_000;
        cc.w_max = 20.0;

        cc.enter_congestion_recovery(sent, sent + Duration::from_millis(1));

        assert!(cc.w_max < 10.0, "fast convergence should reduce prior peak");
    }

    #[test]
    fn cubic_increment_scales_with_acked_bytes() {
        let now = Instant::now();
        let ack_time = now + Duration::from_secs(1);

        let mut one_mss = Cubic::new();
        one_mss.cwnd = 6_000;
        one_mss.ssthresh = 4_000;
        one_mss.epoch_start = Some(now);
        one_mss.w_max = 10.0;
        one_mss.k = 0.0;
        one_mss.bytes_in_flight = 1_200;

        let mut two_mss = Cubic::new();
        two_mss.cwnd = 6_000;
        two_mss.ssthresh = 4_000;
        two_mss.epoch_start = Some(now);
        two_mss.w_max = 10.0;
        two_mss.k = 0.0;
        two_mss.bytes_in_flight = 2_400;

        one_mss.on_ack(1_200, Duration::from_millis(50), now, ack_time);
        two_mss.on_ack(2_400, Duration::from_millis(50), now, ack_time);

        assert!(two_mss.window() > one_mss.window());
    }

    #[test]
    fn persistent_congestion_resets() {
        let mut cc = Cubic::new();
        cc.on_persistent_congestion();
        assert_eq!(cc.window(), MINIMUM_WINDOW);
    }

    #[test]
    fn k_computation() {
        // K should be positive for positive W_max
        let k = Cubic::compute_k(10.0);
        assert!(k > 0.0);
    }

    #[test]
    fn congestion_avoidance_grows_slowly() {
        let mut cc = Cubic::new();
        let now = Instant::now();
        let rtt = Duration::from_millis(50);

        // Force into congestion avoidance by triggering a loss
        cc.on_packet_sent(6000, now);
        cc.on_loss(1200, now, now + Duration::from_millis(1));
        let window_after_loss = cc.window();
        assert!(cc.window() < INITIAL_WINDOW);
        assert!(cc.ssthresh() < usize::MAX); // ssthresh set

        // Send/ack packets in congestion avoidance
        for i in 0..20 {
            let sent_time = now + Duration::from_millis(100 + i * 50);
            cc.on_packet_sent(1200, sent_time);
            let ack_time = sent_time + rtt;
            cc.on_ack(1200, rtt, sent_time, ack_time);
        }

        // Window should grow, but slower than slow start (where it would grow by 1200 per ack)
        let total_growth = cc.window() - window_after_loss;
        assert!(
            total_growth > 0,
            "congestion avoidance should grow the window"
        );
        assert!(
            total_growth < 20 * 1200,
            "congestion avoidance growth ({total_growth}) should be slower than slow start ({})",
            20 * 1200
        );
    }

    #[test]
    fn on_loss_uses_sent_time_for_recovery() {
        let mut cc = Cubic::new();
        let now = Instant::now();

        cc.on_packet_sent(6000, now);

        // First loss at t=1ms (sent_time=now, detection_time=now+1ms)
        cc.on_loss(1200, now, now + Duration::from_millis(1));
        let window1 = cc.window();

        // Second loss for a packet also sent at t=0 (same recovery period)
        // Should NOT reduce the window again since sent_time <= congestion_recovery_start
        cc.on_loss(1200, now, now + Duration::from_millis(2));
        assert_eq!(
            cc.window(),
            window1,
            "loss within recovery period should not reduce cwnd"
        );
    }
}
