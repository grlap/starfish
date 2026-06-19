//! Loss detection and persistent congestion (RFC 9002 §6, §7.6).
//!
//! Implements time-based and packet-threshold loss detection with
//! Probe Timeout (PTO) for robustness against tail losses.
//! Tracks ack-eliciting packet send times across ACK rounds so that
//! persistent congestion detection accounts for previously-acknowledged
//! packets (not just the current ACK batch).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use starfish_reactor::cooperative_io::udp_socket::UdpEcnCodepoint;

use crate::packet::header::PacketNumberSpace;
use crate::packet::DEFAULT_MAX_ACK_DELAY_MS;

/// Number of packet number spaces (Initial, Handshake, ApplicationData).
const NUM_SPACES: usize = 3;

/// Packet loss threshold in number of packets (RFC 9002 §6.1.1).
const PACKET_THRESHOLD: u64 = 3;

/// Time threshold multiplier (RFC 9002 §6.1.2).
const TIME_THRESHOLD_MULTIPLIER: f64 = 9.0 / 8.0;

/// Persistent congestion threshold in PTOs (RFC 9002 §7.6.2).
const PERSISTENT_CONGESTION_THRESHOLD: u32 = 3;

/// Initial RTT estimate (RFC 9002 §6.2.2).
pub const INITIAL_RTT: Duration = Duration::from_millis(333);

/// Information about a sent packet, for loss detection tracking.
#[derive(Debug, Clone)]
pub struct SentPacket {
    /// Packet number.
    pub pn: u64,
    /// Packet number space.
    pub space: PacketNumberSpace,
    /// Time the packet was sent.
    pub time_sent: Instant,
    /// Size in bytes (for congestion control).
    pub size: usize,
    /// Whether this packet is ack-eliciting.
    pub ack_eliciting: bool,
    /// ECN codepoint used when sending this packet, if any.
    pub ecn_marking: Option<UdpEcnCodepoint>,
}

/// RTT estimator state.
#[derive(Debug)]
pub struct RttEstimator {
    /// Smoothed RTT (RFC 9002 §5.3).
    pub smoothed_rtt: Duration,
    /// RTT variation (RFC 9002 §5.3).
    pub rttvar: Duration,
    /// Minimum RTT observed.
    pub min_rtt: Duration,
    /// Most recent RTT sample.
    pub latest_rtt: Duration,
    /// Whether we've taken the first RTT sample.
    first_sample: bool,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RttEstimator {
    pub fn new() -> Self {
        Self {
            smoothed_rtt: INITIAL_RTT,
            rttvar: INITIAL_RTT / 2,
            min_rtt: Duration::MAX,
            latest_rtt: Duration::ZERO,
            first_sample: true,
        }
    }

    /// Update RTT estimates with a new sample (RFC 9002 §5.3).
    pub fn update(&mut self, rtt_sample: Duration, ack_delay: Duration) {
        self.latest_rtt = rtt_sample;
        self.min_rtt = self.min_rtt.min(rtt_sample);

        if self.first_sample {
            self.smoothed_rtt = rtt_sample;
            self.rttvar = rtt_sample / 2;
            self.first_sample = false;
            return;
        }

        // Adjust for ack delay, but don't let it reduce below min_rtt
        let adjusted_rtt = if rtt_sample > self.min_rtt + ack_delay {
            rtt_sample - ack_delay
        } else {
            rtt_sample
        };

        // rttvar = 3/4 * rttvar + 1/4 * |smoothed_rtt - adjusted_rtt|
        let diff = self.smoothed_rtt.abs_diff(adjusted_rtt);
        self.rttvar = (self.rttvar * 3 + diff) / 4;

        // smoothed_rtt = 7/8 * smoothed_rtt + 1/8 * adjusted_rtt
        self.smoothed_rtt = (self.smoothed_rtt * 7 + adjusted_rtt) / 8;
    }

    /// Probe Timeout duration (RFC 9002 §6.2.1).
    pub fn pto(&self) -> Duration {
        self.smoothed_rtt + self.rttvar.max(Duration::from_millis(1)) * 4
    }
}

/// Per-space loss detection state.
#[derive(Debug)]
struct SpaceState {
    /// Largest acknowledged packet number.
    largest_acked: Option<u64>,
    /// Time at which the next packet in this space is considered lost.
    loss_time: Option<Instant>,
    /// Sent packets, keyed by packet number.
    sent_packets: BTreeMap<u64, SentPacket>,
    /// Send times of all previously acknowledged ack-eliciting packets.
    /// Used by persistent congestion detection to account for packets
    /// that were acked in prior ACK rounds (RFC 9002 §7.6.2).
    acked_ack_eliciting_times: BTreeSet<Instant>,
    /// Time of the last ack-eliciting packet sent in this space.
    time_of_last_ack_eliciting: Option<Instant>,
}

impl SpaceState {
    fn new() -> Self {
        Self {
            largest_acked: None,
            loss_time: None,
            sent_packets: BTreeMap::new(),
            acked_ack_eliciting_times: BTreeSet::new(),
            time_of_last_ack_eliciting: None,
        }
    }
}

/// Loss detection state for a QUIC connection.
#[derive(Debug)]
pub struct LossDetection {
    spaces: [SpaceState; NUM_SPACES],
    pub rtt: RttEstimator,
    /// PTO count (number of consecutive PTOs without receiving an ack).
    pub pto_count: u32,
    /// Negotiated max_ack_delay from the peer transport parameters.
    max_ack_delay: Duration,
}

impl Default for LossDetection {
    fn default() -> Self {
        Self::new()
    }
}

impl LossDetection {
    pub fn new() -> Self {
        Self {
            spaces: [SpaceState::new(), SpaceState::new(), SpaceState::new()],
            rtt: RttEstimator::new(),
            pto_count: 0,
            max_ack_delay: Duration::from_millis(DEFAULT_MAX_ACK_DELAY_MS),
        }
    }

    pub fn set_max_ack_delay(&mut self, max_ack_delay: Duration) {
        self.max_ack_delay = max_ack_delay;
    }

    /// Record a sent packet.
    pub fn on_packet_sent(&mut self, pkt: SentPacket) {
        if !pkt.ack_eliciting {
            return;
        }

        let space = pkt.space as usize;
        self.spaces[space].time_of_last_ack_eliciting = Some(pkt.time_sent);
        self.spaces[space].sent_packets.insert(pkt.pn, pkt);
    }

    /// Process an ACK frame. Returns the list of newly acknowledged packets
    /// and the list of packets detected as lost.
    pub fn on_ack_received(
        &mut self,
        space: PacketNumberSpace,
        largest_acked: u64,
        ack_delay: Duration,
        ack_ranges: &[(u64, u64)], // (start, end) inclusive
        now: Instant,
    ) -> (Vec<SentPacket>, Vec<SentPacket>) {
        let s = space as usize;

        // Update largest acked
        let is_new_largest = match self.spaces[s].largest_acked {
            Some(prev) => largest_acked > prev,
            None => true,
        };
        if is_new_largest {
            self.spaces[s].largest_acked = Some(largest_acked);
        }

        // Find newly acknowledged packets
        let mut newly_acked = Vec::new();
        for &(start, end) in ack_ranges {
            let range = start..=end;
            let pns: Vec<u64> = self.spaces[s]
                .sent_packets
                .range(range)
                .map(|(&pn, _)| pn)
                .collect();
            for pn in pns {
                if let Some(pkt) = self.spaces[s].sent_packets.remove(&pn) {
                    newly_acked.push(pkt);
                }
            }
        }

        // Record ack-eliciting send times for persistent congestion checks
        for pkt in &newly_acked {
            if pkt.ack_eliciting {
                self.spaces[s]
                    .acked_ack_eliciting_times
                    .insert(pkt.time_sent);
            }
        }

        // Update RTT from the largest acknowledged packet
        if is_new_largest {
            if let Some(largest_pkt) = newly_acked.iter().find(|p| p.pn == largest_acked) {
                let rtt_sample = now.duration_since(largest_pkt.time_sent);
                self.rtt.update(rtt_sample, ack_delay);
            }
        }

        // Reset PTO count on successful ack
        self.pto_count = 0;

        // Detect lost packets
        let lost = self.detect_lost_packets(space, now);

        (newly_acked, lost)
    }

    /// Detect lost packets using time and packet thresholds (RFC 9002 §6.1).
    fn detect_lost_packets(&mut self, space: PacketNumberSpace, now: Instant) -> Vec<SentPacket> {
        let s = space as usize;
        let largest_acked = match self.spaces[s].largest_acked {
            Some(la) => la,
            None => return vec![],
        };

        let loss_delay = Duration::from_secs_f64(
            self.rtt.latest_rtt.max(self.rtt.smoothed_rtt).as_secs_f64()
                * TIME_THRESHOLD_MULTIPLIER,
        );
        let loss_delay = loss_delay.max(Duration::from_millis(1));
        let lost_send_time = now.checked_sub(loss_delay);

        let mut lost = Vec::new();
        self.spaces[s].loss_time = None;

        let pns: Vec<u64> = self.spaces[s]
            .sent_packets
            .keys()
            .copied()
            .filter(|&pn| pn < largest_acked)
            .collect();

        for pn in pns {
            let pkt = match self.spaces[s].sent_packets.get(&pn) {
                Some(p) => p,
                None => continue,
            };

            // Packet threshold: lost if more than PACKET_THRESHOLD packets
            // have been acknowledged after it
            let packet_threshold_met = largest_acked - pn >= PACKET_THRESHOLD;

            // Time threshold: lost if sent more than loss_delay ago
            let time_threshold_met = lost_send_time
                .map(|lt| pkt.time_sent <= lt)
                .unwrap_or(false);

            if packet_threshold_met || time_threshold_met {
                if let Some(pkt) = self.spaces[s].sent_packets.remove(&pn) {
                    lost.push(pkt);
                }
            } else {
                // Set loss timer for this packet
                let loss_time = pkt.time_sent + loss_delay;
                match self.spaces[s].loss_time {
                    Some(existing) if existing <= loss_time => {}
                    _ => self.spaces[s].loss_time = Some(loss_time),
                }
            }
        }

        lost
    }

    /// Get the earliest loss time across all spaces.
    #[allow(dead_code)]
    pub fn earliest_loss_time(&self) -> Option<Instant> {
        self.spaces.iter().filter_map(|s| s.loss_time).min()
    }

    /// Get the next PTO deadline, if there are any ack-eliciting packets in flight.
    pub fn pto_deadline(&self) -> Option<Instant> {
        [
            PacketNumberSpace::Initial,
            PacketNumberSpace::Handshake,
            PacketNumberSpace::ApplicationData,
        ]
        .into_iter()
        .filter_map(|space| {
            let state = &self.spaces[space as usize];
            (!state.sent_packets.is_empty())
                .then_some(state.time_of_last_ack_eliciting)
                .flatten()
                .map(|sent_at| sent_at + self.pto_timeout_for_space(space))
        })
        .min()
    }

    fn pto_timeout_for_space(&self, space: PacketNumberSpace) -> Duration {
        let mut pto = self.rtt.pto();
        if matches!(space, PacketNumberSpace::ApplicationData) {
            pto += self.max_ack_delay;
        }
        pto * (1u32 << self.pto_count.min(30))
    }

    /// Get the PTO timeout (RFC 9002 §6.2.1).
    pub fn pto_timeout(&self) -> Duration {
        let mut pto = self.rtt.pto();
        if self.has_unacked(PacketNumberSpace::ApplicationData) {
            pto += self.max_ack_delay;
        }
        pto *= 1u32 << self.pto_count.min(30);
        pto
    }

    fn persistent_congestion_duration(&self, space: PacketNumberSpace) -> Duration {
        let mut pto = self.rtt.pto();
        if matches!(space, PacketNumberSpace::ApplicationData) {
            pto += self.max_ack_delay;
        }
        pto * PERSISTENT_CONGESTION_THRESHOLD
    }

    /// Returns true when the newly lost ack-eliciting packets span a
    /// persistent congestion interval and there are no acknowledged or still
    /// outstanding ack-eliciting packets inside that interval.
    ///
    /// Per RFC 9002 §7.6.2, "none of the packets sent between them are
    /// acknowledged" must account for ALL acknowledged packets — including
    /// those acked in prior ACK rounds, not just the current batch.
    pub fn detects_persistent_congestion(
        &self,
        space: PacketNumberSpace,
        acked: &[SentPacket],
        lost: &[SentPacket],
    ) -> bool {
        let mut lost_ack_eliciting = lost.iter().filter(|pkt| pkt.ack_eliciting);
        let Some(first) = lost_ack_eliciting.next() else {
            return false;
        };

        let mut start = first.time_sent;
        let mut end = first.time_sent;
        for pkt in lost_ack_eliciting {
            start = start.min(pkt.time_sent);
            end = end.max(pkt.time_sent);
        }

        if end.duration_since(start) < self.persistent_congestion_duration(space) {
            return false;
        }

        // Check current ACK batch
        let has_acked_in_interval = acked
            .iter()
            .any(|pkt| pkt.ack_eliciting && pkt.time_sent >= start && pkt.time_sent <= end);
        if has_acked_in_interval {
            return false;
        }

        // Check packets acked in prior ACK rounds
        let s = &self.spaces[space as usize];
        if s.acked_ack_eliciting_times
            .range(start..=end)
            .next()
            .is_some()
        {
            return false;
        }

        // Check still-outstanding packets
        !s.sent_packets
            .values()
            .any(|pkt| pkt.ack_eliciting && pkt.time_sent >= start && pkt.time_sent <= end)
    }

    /// Drop acknowledged send timestamps that can no longer participate in a
    /// future persistent-congestion interval for this space.
    pub fn prune_acknowledged_ack_eliciting_times(&mut self, space: PacketNumberSpace) {
        let s = &mut self.spaces[space as usize];
        let Some(oldest_outstanding_ack_eliciting) = s
            .sent_packets
            .values()
            .filter(|pkt| pkt.ack_eliciting)
            .map(|pkt| pkt.time_sent)
            .min()
        else {
            s.acked_ack_eliciting_times.clear();
            return;
        };

        s.acked_ack_eliciting_times = s
            .acked_ack_eliciting_times
            .split_off(&oldest_outstanding_ack_eliciting);
    }

    /// Increment PTO count (called when PTO fires without receiving an ACK).
    pub fn on_pto_timeout(&mut self) {
        self.pto_count += 1;
    }

    /// Discard all state for a packet number space (called when keys are discarded).
    #[cfg(test)]
    pub fn discard_space(&mut self, space: PacketNumberSpace) {
        let s = space as usize;
        self.spaces[s] = SpaceState::new();
    }

    /// Get the largest acknowledged packet number for a space.
    pub fn largest_acked(&self, space: PacketNumberSpace) -> Option<u64> {
        self.spaces[space as usize].largest_acked
    }

    /// Check if there are any unacknowledged packets in a space.
    pub fn has_unacked(&self, space: PacketNumberSpace) -> bool {
        !self.spaces[space as usize].sent_packets.is_empty()
    }

    /// Remove a tracked in-flight packet without waiting for ACK processing.
    pub fn remove_sent_packet(&mut self, space: PacketNumberSpace, pn: u64) -> Option<SentPacket> {
        self.spaces[space as usize].sent_packets.remove(&pn)
    }

    /// Remove and return all tracked packets in a packet number space.
    pub fn take_sent_packets(&mut self, space: PacketNumberSpace) -> Vec<SentPacket> {
        std::mem::take(&mut self.spaces[space as usize].sent_packets)
            .into_values()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent_pkt(pn: u64, space: PacketNumberSpace, time: Instant) -> SentPacket {
        SentPacket {
            pn,
            space,
            time_sent: time,
            size: 1200,
            ack_eliciting: true,
            ecn_marking: None,
        }
    }

    fn non_ack_eliciting_pkt(pn: u64, space: PacketNumberSpace, time: Instant) -> SentPacket {
        SentPacket {
            pn,
            space,
            time_sent: time,
            size: 64,
            ack_eliciting: false,
            ecn_marking: None,
        }
    }

    #[test]
    fn rtt_estimator_first_sample() {
        let mut rtt = RttEstimator::new();
        rtt.update(Duration::from_millis(100), Duration::ZERO);
        assert_eq!(rtt.smoothed_rtt, Duration::from_millis(100));
        assert_eq!(rtt.rttvar, Duration::from_millis(50));
        assert_eq!(rtt.min_rtt, Duration::from_millis(100));
    }

    #[test]
    fn rtt_estimator_convergence() {
        let mut rtt = RttEstimator::new();
        // Feed consistent 100ms samples
        for _ in 0..10 {
            rtt.update(Duration::from_millis(100), Duration::ZERO);
        }
        // smoothed_rtt should converge near 100ms
        let diff = if rtt.smoothed_rtt > Duration::from_millis(100) {
            rtt.smoothed_rtt - Duration::from_millis(100)
        } else {
            Duration::from_millis(100) - rtt.smoothed_rtt
        };
        assert!(diff < Duration::from_millis(5));
    }

    #[test]
    fn pto_calculation() {
        let rtt = RttEstimator::new();
        let pto = rtt.pto();
        // PTO = smoothed_rtt + max(4 * rttvar, 1ms)
        assert!(pto > Duration::ZERO);
    }

    #[test]
    fn basic_ack_processing() {
        let mut ld = LossDetection::new();
        let now = Instant::now();

        for pn in 0..5u64 {
            ld.on_packet_sent(sent_pkt(pn, PacketNumberSpace::ApplicationData, now));
        }

        // ACK packets 0-4
        let (acked, lost) = ld.on_ack_received(
            PacketNumberSpace::ApplicationData,
            4,
            Duration::ZERO,
            &[(0, 4)],
            now + Duration::from_millis(100),
        );

        assert_eq!(acked.len(), 5);
        assert!(lost.is_empty());
    }

    #[test]
    fn packet_threshold_loss() {
        let mut ld = LossDetection::new();
        let now = Instant::now();

        // Send packets 0..6
        for pn in 0..6u64 {
            ld.on_packet_sent(sent_pkt(
                pn,
                PacketNumberSpace::ApplicationData,
                now + Duration::from_millis(pn * 10),
            ));
        }

        // ACK only packets 3, 4, 5 (skip 0, 1, 2)
        let (acked, lost) = ld.on_ack_received(
            PacketNumberSpace::ApplicationData,
            5,
            Duration::ZERO,
            &[(3, 5)],
            now + Duration::from_millis(100),
        );

        assert_eq!(acked.len(), 3);
        // Packets 0, 1, 2 should be detected as lost (> PACKET_THRESHOLD gap)
        assert_eq!(lost.len(), 3); // pn 0, 1, 2 (5 - pn >= 3)
    }

    #[test]
    fn discard_space() {
        let mut ld = LossDetection::new();
        let now = Instant::now();

        ld.on_packet_sent(sent_pkt(0, PacketNumberSpace::Initial, now));
        assert!(ld.has_unacked(PacketNumberSpace::Initial));

        ld.discard_space(PacketNumberSpace::Initial);
        assert!(!ld.has_unacked(PacketNumberSpace::Initial));
    }

    #[test]
    fn pto_backoff() {
        let mut ld = LossDetection::new();
        let base_pto = ld.pto_timeout();

        ld.on_pto_timeout();
        assert_eq!(ld.pto_timeout(), base_pto * 2);

        ld.on_pto_timeout();
        assert_eq!(ld.pto_timeout(), base_pto * 4);
    }

    #[test]
    fn pto_includes_negotiated_max_ack_delay_for_application_data() {
        let mut ld = LossDetection::new();
        let now = Instant::now();
        ld.set_max_ack_delay(Duration::from_millis(37));
        ld.on_packet_sent(sent_pkt(0, PacketNumberSpace::ApplicationData, now));

        assert_eq!(ld.pto_timeout(), ld.rtt.pto() + Duration::from_millis(37));
    }

    #[test]
    fn pto_ignores_max_ack_delay_without_application_data_in_flight() {
        let mut ld = LossDetection::new();
        let now = Instant::now();
        ld.set_max_ack_delay(Duration::from_millis(37));
        ld.on_packet_sent(sent_pkt(0, PacketNumberSpace::Handshake, now));

        assert_eq!(ld.pto_timeout(), ld.rtt.pto());
    }

    #[test]
    fn pto_deadline_tracks_last_ack_eliciting_send_time() {
        let mut ld = LossDetection::new();
        let now = Instant::now();
        ld.on_packet_sent(sent_pkt(0, PacketNumberSpace::Initial, now));

        assert_eq!(ld.pto_deadline(), Some(now + ld.pto_timeout()));
    }

    #[test]
    fn pto_deadline_uses_earliest_per_space_deadline() {
        let mut ld = LossDetection::new();
        let now = Instant::now();
        ld.set_max_ack_delay(Duration::from_millis(37));

        ld.on_packet_sent(sent_pkt(0, PacketNumberSpace::ApplicationData, now));
        ld.on_packet_sent(sent_pkt(
            1,
            PacketNumberSpace::Handshake,
            now + Duration::from_millis(1),
        ));

        assert_eq!(
            ld.pto_deadline(),
            Some(now + Duration::from_millis(1) + ld.rtt.pto())
        );
    }

    #[test]
    fn pto_deadline_ignores_non_ack_eliciting_packets() {
        let mut ld = LossDetection::new();
        let now = Instant::now();
        ld.on_packet_sent(non_ack_eliciting_pkt(
            0,
            PacketNumberSpace::ApplicationData,
            now,
        ));

        assert_eq!(ld.pto_deadline(), None);
        assert!(!ld.has_unacked(PacketNumberSpace::ApplicationData));
    }

    #[test]
    fn detects_persistent_congestion_for_fully_lost_run_spanning_three_ptos() {
        let mut ld = LossDetection::new();
        ld.rtt.smoothed_rtt = Duration::from_millis(10);
        ld.rtt.latest_rtt = Duration::from_millis(10);
        ld.rtt.rttvar = Duration::ZERO;
        ld.set_max_ack_delay(Duration::ZERO);

        let base = Instant::now() - Duration::from_millis(120);
        for (pn, offset_ms) in [
            (0, 0),
            (1, 20),
            (2, 44),
            (3, 60),
            (4, 80),
            (5, 100),
            (6, 120),
        ] {
            ld.on_packet_sent(sent_pkt(
                pn,
                PacketNumberSpace::ApplicationData,
                base + Duration::from_millis(offset_ms),
            ));
        }

        let (acked, lost) = ld.on_ack_received(
            PacketNumberSpace::ApplicationData,
            6,
            Duration::ZERO,
            &[(6, 6)],
            base + Duration::from_millis(130),
        );

        assert_eq!(acked.len(), 1);
        assert!(ld.detects_persistent_congestion(
            PacketNumberSpace::ApplicationData,
            &acked,
            &lost,
        ));
    }

    #[test]
    fn acked_packet_inside_loss_window_prevents_persistent_congestion() {
        let mut ld = LossDetection::new();
        ld.rtt.smoothed_rtt = Duration::from_millis(10);
        ld.rtt.latest_rtt = Duration::from_millis(10);
        ld.rtt.rttvar = Duration::ZERO;
        ld.set_max_ack_delay(Duration::ZERO);

        let base = Instant::now() - Duration::from_millis(100);
        for (pn, offset_ms) in [(0, 0), (1, 20), (2, 44), (3, 60), (4, 80), (5, 100)] {
            ld.on_packet_sent(sent_pkt(
                pn,
                PacketNumberSpace::ApplicationData,
                base + Duration::from_millis(offset_ms),
            ));
        }

        let (acked, lost) = ld.on_ack_received(
            PacketNumberSpace::ApplicationData,
            5,
            Duration::ZERO,
            &[(2, 2), (5, 5)],
            base + Duration::from_millis(110),
        );

        assert_eq!(acked.len(), 2);
        assert!(!ld.detects_persistent_congestion(
            PacketNumberSpace::ApplicationData,
            &acked,
            &lost,
        ));
    }

    #[test]
    fn previously_acked_packet_prevents_persistent_congestion() {
        let mut ld = LossDetection::new();
        ld.rtt.smoothed_rtt = Duration::from_millis(10);
        ld.rtt.latest_rtt = Duration::from_millis(10);
        ld.rtt.rttvar = Duration::ZERO;
        ld.set_max_ack_delay(Duration::ZERO);

        // PTO = 10ms, persistent congestion = 3 * 10ms = 30ms.
        // Send packets 0..7 over 120ms — well beyond the threshold.
        let base = Instant::now() - Duration::from_millis(200);
        for (pn, offset_ms) in [
            (0, 0),
            (1, 20),
            (2, 40),
            (3, 60),
            (4, 80),
            (5, 100),
            (6, 120),
            (7, 140),
        ] {
            ld.on_packet_sent(sent_pkt(
                pn,
                PacketNumberSpace::ApplicationData,
                base + Duration::from_millis(offset_ms),
            ));
        }

        // First ACK round: ack packet 3 (sent at t=60ms, inside the interval).
        // This removes pkt 3 from sent_packets and records its send time.
        let (_acked1, _lost1) = ld.on_ack_received(
            PacketNumberSpace::ApplicationData,
            3,
            Duration::ZERO,
            &[(3, 3)],
            base + Duration::from_millis(150),
        );
        // pkt 0 declared lost by packet threshold (3 - 0 = 3 >= 3)

        // Second ACK round: ack packet 7, declaring 1,2,4,5,6 lost.
        let (acked2, lost2) = ld.on_ack_received(
            PacketNumberSpace::ApplicationData,
            7,
            Duration::ZERO,
            &[(7, 7)],
            base + Duration::from_millis(160),
        );

        assert_eq!(acked2.len(), 1);
        // The lost packets span 20ms..120ms (100ms > 30ms threshold).
        // Without tracking prior acks, pkt 3 (at t=60ms) would be invisible
        // and persistent congestion would be falsely declared.
        assert!(
            !ld.detects_persistent_congestion(PacketNumberSpace::ApplicationData, &acked2, &lost2,),
            "previously-acked packet 3 should prevent persistent congestion"
        );
    }

    #[test]
    fn prune_acknowledged_ack_eliciting_times_preserves_relevant_history() {
        let mut ld = LossDetection::new();
        let base = Instant::now();
        let app = PacketNumberSpace::ApplicationData as usize;
        ld.spaces[app]
            .acked_ack_eliciting_times
            .insert(base + Duration::from_millis(0));
        ld.spaces[app]
            .acked_ack_eliciting_times
            .insert(base + Duration::from_millis(10));
        ld.spaces[app].sent_packets.insert(
            7,
            sent_pkt(
                7,
                PacketNumberSpace::ApplicationData,
                base + Duration::from_millis(5),
            ),
        );

        ld.prune_acknowledged_ack_eliciting_times(PacketNumberSpace::ApplicationData);

        let acked_times = &ld.spaces[app].acked_ack_eliciting_times;
        assert_eq!(acked_times.len(), 1);
        assert!(acked_times.contains(&(base + Duration::from_millis(10))));

        ld.spaces[app].sent_packets.clear();
        ld.prune_acknowledged_ack_eliciting_times(PacketNumberSpace::ApplicationData);

        assert!(ld.spaces[app].acked_ack_eliciting_times.is_empty());
    }
}
