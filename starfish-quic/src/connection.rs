//! QUIC connection (RFC 9000).
//!
//! [`QuicConnection`] manages the full lifecycle of a QUIC connection:
//! handshake, packet I/O, stream management, flow control, and loss recovery.
//! It wraps a `UdpSocket` and integrates with rustls for TLS 1.3.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rustls::quic;
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;
use starfish_reactor::cooperative_io::udp_socket::{UdpEcnCodepoint, UdpSocket};
use starfish_reactor::cooperative_synchronization::delayed_future::cooperative_sleep;

use crate::crypto::aead;
use crate::crypto::header_protection;
use crate::crypto::keys::{self, PacketKeys};
use crate::error::{QuicError, TransportErrorCode};
use crate::frame::ack::{AckFrame, AckRange, EcnCounts};
use crate::frame::crypto::CryptoFrame;
use crate::frame::stream::StreamFrame;
use crate::frame::{self, Frame};
use crate::packet::header::{LongPacketType, PacketHeader, PacketNumberSpace};
use crate::packet::retry;
use crate::packet::{self, ConnectionId};
use crate::recovery::congestion::CongestionController;
use crate::recovery::cubic::Cubic;
use crate::recovery::loss_detection::{LossDetection, SentPacket};
use crate::transport::connection_id::ConnectionIdManager;
use crate::transport::flow_control::ConnectionFlowControl;
use crate::transport::stream_manager::{
    RecvState, SendState, Side, StreamId, StreamManager, StreamState,
};

/// QUIC connection state (RFC 9000 §Appendix A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Sending/receiving Initial packets.
    Initial,
    /// Exchanging Handshake packets.
    Handshake,
    /// Handshake complete, exchanging application data.
    Connected,
    /// Sent CONNECTION_CLOSE, waiting for the closing period to expire.
    Closing,
    /// Received CONNECTION_CLOSE, draining period.
    Draining,
    /// Connection fully closed.
    Closed,
}

/// Default initial flow control window.
pub(crate) const DEFAULT_INITIAL_MAX_DATA: u64 = 1_048_576; // 1 MB
pub(crate) const DEFAULT_INITIAL_MAX_STREAM_DATA: u64 = 262_144; // 256 KB
pub(crate) const DEFAULT_INITIAL_MAX_STREAMS_BIDI: u64 = 100;
pub(crate) const DEFAULT_INITIAL_MAX_STREAMS_UNI: u64 = 100;

/// Default idle timeout in milliseconds (RFC 9000 §10.1).
pub(crate) const DEFAULT_IDLE_TIMEOUT_MS: u64 = 30_000; // 30 seconds

/// PING keep-alive is sent when this fraction of the idle timeout has elapsed.
const KEEP_ALIVE_FRACTION: u32 = 2; // send at 1/2 of idle timeout

/// Maximum UDP datagram size.
pub(crate) const MAX_DATAGRAM_SIZE: usize = 1472;
/// Maximum UDP payload size we can receive and advertise locally.
pub(crate) const MAX_LOCAL_UDP_PAYLOAD_SIZE: usize = packet::DEFAULT_MAX_UDP_PAYLOAD_SIZE as usize;

/// Conservative estimate of QUIC packet overhead (header + PN + AEAD tag).
/// Initial: ~50B, Handshake: ~45B, 1-RTT: ~25B. Use worst case + margin.
const PACKET_OVERHEAD: usize = 100;

/// Default ACK delay exponent (RFC 9000 §18.2).
const DEFAULT_ACK_DELAY_EXPONENT: u32 = 3;
/// Default active connection ID limit (RFC 9000 §18.2).
const DEFAULT_ACTIVE_CONNECTION_ID_LIMIT: u64 = 2;
const MAX_STREAMS_LIMIT: u64 = 1 << 60;
const MAX_TRACKED_RECV_PN_RANGES: usize = 32;
/// Stateless reset token size (RFC 9000 §10.3).
pub(crate) const STATELESS_RESET_TOKEN_LEN: usize = 16;
/// Minimum valid stateless reset datagram length.
const MIN_STATELESS_RESET_LEN: usize = 1 + 4 + STATELESS_RESET_TOKEN_LEN;
/// Rotate keys slightly before the AEAD confidentiality limit is reached.
const KEY_UPDATE_SAFETY_MARGIN: u64 = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingPeerPathValidation {
    peer_addr: SocketAddr,
    challenge_data: [u8; 8],
    kind: PathValidationKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathValidationKind {
    ObservedPeerMigration,
    PreferredAddress(PreferredPeerAddress),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreferredPeerAddress {
    peer_addr: SocketAddr,
    connection_id: ConnectionId,
    stateless_reset_token: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PeerPathState {
    bytes_received: usize,
    bytes_sent: usize,
    validated: bool,
}

struct PendingRemoteKeyUpdate {
    key_phase: bool,
    remote: Box<dyn quic::PacketKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PathMtuProbe {
    pn: u64,
    size: usize,
    sent_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PathMtuState {
    peer_max_udp_payload_size: usize,
    local_max_udp_payload_size: usize,
    probe_upper_bound: usize,
    in_flight_probe: Option<PathMtuProbe>,
    next_probe_at: Option<Instant>,
}

impl PathMtuState {
    fn new() -> Self {
        Self {
            peer_max_udp_payload_size: MAX_DATAGRAM_SIZE,
            local_max_udp_payload_size: MAX_LOCAL_UDP_PAYLOAD_SIZE,
            probe_upper_bound: MAX_LOCAL_UDP_PAYLOAD_SIZE,
            in_flight_probe: None,
            next_probe_at: None,
        }
    }

    fn search_limit(&self) -> usize {
        self.local_max_udp_payload_size
            .min(self.peer_max_udp_payload_size)
            .min(self.probe_upper_bound)
    }
}

struct BuiltPacket {
    space: PacketNumberSpace,
    pn: u64,
    frames: Vec<Frame>,
    bytes: Vec<u8>,
    ack_eliciting: bool,
    track_retransmission: bool,
    congestion_controlled: bool,
    app_kind: Option<ApplicationPacketKind>,
}

type PeerTransportParameterCache = Arc<Mutex<HashMap<String, packet::PeerTransportParameters>>>;
type ClientTokenCache = Arc<Mutex<HashMap<String, Vec<u8>>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplicationPacketKind {
    ZeroRtt,
    OneRtt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceivedPacketKind {
    Initial,
    Handshake,
    ZeroRtt,
    OneRtt,
}

/// A QUIC connection.
pub struct QuicConnection {
    /// Current connection state.
    pub(crate) state: ConnectionState,
    /// Which side we are (client or server).
    pub(crate) side: Side,
    /// The UDP socket for sending and receiving packets.
    pub(crate) socket: UdpSocket,
    /// Remote peer address.
    pub(crate) peer_addr: SocketAddr,
    /// TLS state (rustls QUIC-mode connection).
    pub(crate) tls: QuicTls,
    /// Packet protection keys.
    pub(crate) keys: PacketKeys,
    /// Connection ID management.
    pub(crate) cid_manager: ConnectionIdManager,
    /// Stream management.
    pub(crate) streams: StreamManager,
    /// Connection-level flow control.
    pub(crate) flow_control: ConnectionFlowControl,
    /// Loss detection.
    pub(crate) loss_detection: LossDetection,
    /// Congestion controller.
    pub(crate) congestion: Box<dyn CongestionController>,
    /// Next packet number for each space.
    pub(crate) next_pn: [u64; 3],
    /// Largest packet number received for each space (for decoding truncated PNs).
    pub(crate) largest_recv_pn: [u64; 3],
    /// Arrival time of the largest packet received in each space.
    largest_recv_pn_time: [Option<Instant>; 3],
    /// Whether a packet number has been received in each space yet.
    received_packets: [bool; 3],
    /// Pending frames to send.
    pub(crate) pending_frames: VecDeque<(PacketNumberSpace, Frame)>,
    /// Pending frames that must be sent to a specific path instead of `peer_addr`.
    pending_targeted_frames: VecDeque<(SocketAddr, PacketNumberSpace, Frame)>,
    /// Whether we need to send an ACK.
    pub(crate) ack_needed: [bool; 3],
    /// Number of ack-eliciting packets received since the last ACK was sent per space.
    ack_eliciting_packets_since_last_ack: [u8; 3],
    /// Whether the next ACK in a space should bypass the delay timer.
    ack_immediate: [bool; 3],
    /// Received packet numbers for generating ACKs.
    pub(crate) recv_pn_ranges: [Vec<(u64, u64)>; 3],
    /// Cumulative ECN counts for successfully received packets per space.
    recv_ecn_counts: [EcnCounts; 3],
    /// Crypto stream reassembly buffers (one per space), keyed by offset.
    pub(crate) crypto_recv_buf: [BTreeMap<u64, Vec<u8>>; 3],
    /// Next expected crypto receive offset per space.
    pub(crate) crypto_recv_offset: [u64; 3],
    /// Next crypto send offset per space.
    pub(crate) crypto_send_offset: [u64; 3],
    /// Frames sent in each packet, keyed by (space_idx, pn) for retransmission on loss.
    sent_frames: [BTreeMap<u64, Vec<Frame>>; 3],
    /// Application-data packets that were protected with 0-RTT keys.
    zero_rtt_in_flight: BTreeSet<u64>,
    /// Time of last ack-eliciting packet received per space (for ACK delay).
    last_ack_eliciting_recv: [Option<Instant>; 3],
    /// Last peer-reported ACK_ECN counters per space.
    peer_ack_ecn_counts: [Option<EcnCounts>; 3],
    /// Cumulative ECN codepoints used on sent ack-eliciting packets per space.
    sent_ecn_counts: [EcnCounts; 3],
    /// Whether outbound application packets should still be marked ECT(0).
    outbound_ecn_enabled: bool,
    /// Peer's ACK delay exponent (RFC 9000 §18.2), default 3.
    peer_ack_delay_exponent: u32,
    /// Peer's max_ack_delay (RFC 9000 §18.2), default 25ms.
    peer_max_ack_delay: Duration,
    /// Our local ACK delay exponent sent to the peer.
    local_ack_delay_exponent: u32,
    /// Maximum delayed-ACK timer we advertise to the peer.
    local_max_ack_delay: Duration,
    /// Maximum UDP payload size the peer is willing to receive.
    max_send_udp_payload_size: usize,
    /// PMTUD state for larger application-data datagram probes.
    path_mtu: PathMtuState,
    /// Key discards deferred until after pending frames are sent.
    deferred_key_discards: Vec<PacketNumberSpace>,
    /// Buffer for receiving UDP datagrams.
    recv_buf: Vec<u8>,
    // ---- QUIC-5: Idle timeout (RFC 9000 §10.1) ----
    /// Idle timeout duration (minimum of local and peer's max_idle_timeout).
    idle_timeout: Duration,
    /// Time of last packet received (or connection creation).
    last_recv_time: Instant,
    /// Timer restart point for QUIC idle-timeout accounting.
    idle_timeout_start: Instant,
    /// Whether we have already sent an ack-eliciting packet since the last receive.
    sent_ack_eliciting_since_last_recv: bool,
    /// Time of last packet sent (for keep-alive scheduling).
    last_send_time: Instant,
    /// Earliest paced send time for the next outbound packet.
    next_paced_send_time: Option<Instant>,
    /// Total bytes received from the peer before its address is validated.
    bytes_received: usize,
    /// Total bytes sent to the peer before its address is validated.
    bytes_sent: usize,
    /// Whether the peer's current address is validated for amplification purposes.
    peer_address_validated: bool,
    /// Server-side anti-amplification and validation state indexed by peer address.
    peer_paths: HashMap<SocketAddr, PeerPathState>,
    /// Deadline for remaining in the closing or draining state.
    draining_until: Option<Instant>,
    /// Stored local CONNECTION_CLOSE frames for closing-state responses.
    closing_frames: Vec<(PacketNumberSpace, Frame)>,
    /// Whether a closing-state retransmission of CONNECTION_CLOSE is queued.
    closing_response_pending: bool,
    /// Remembered peer transport parameters from the resumed session.
    remembered_peer_transport_parameters: Option<packet::PeerTransportParameters>,
    /// Shared client-side cache of peer transport parameters.
    peer_transport_parameter_cache: Option<PeerTransportParameterCache>,
    /// Cache key used for remembered peer transport parameters.
    peer_transport_parameter_cache_key: Option<String>,
    /// Shared client-side cache of NEW_TOKEN values keyed by server name.
    client_token_cache: Option<ClientTokenCache>,
    /// Whether the peer forbids active migration away from the current path.
    peer_disable_active_migration: bool,
    /// Server-advertised preferred address information, if any.
    peer_preferred_address: Option<PreferredPeerAddress>,
    /// Whether this client has actually sent any 0-RTT application data.
    attempted_zero_rtt: bool,
    /// Whether the peer accepted sent 0-RTT data, once known.
    early_data_accepted: Option<bool>,
    // ---- QUIC-6: Endpoint-managed mode ----
    /// When true, poll() skips recv_from (the endpoint routes datagrams to us).
    pub(crate) endpoint_managed: bool,
    /// Datagrams queued by the endpoint for processing.
    pub(crate) incoming_datagrams: VecDeque<Vec<u8>>,
    // ---- QUIC-10: Key update rotation (RFC 9001 §6) ----
    /// Next-generation TLS secrets for deriving updated 1-RTT keys.
    next_secrets: Option<rustls::quic::Secrets>,
    /// Current key phase bit used for locally-sent 1-RTT packets.
    local_key_phase: bool,
    /// Current key phase expected for peer-sent 1-RTT packets.
    remote_key_phase: bool,
    /// A locally-initiated key update awaiting the peer's matching key phase.
    pending_remote_key_update: Option<PendingRemoteKeyUpdate>,
    /// Number of 1-RTT packets sent with the current local keys.
    one_rtt_packets_sent_with_current_keys: u64,
    /// The PN at which we accepted the current receive key phase.
    key_update_pn: u64,
    // ---- QUIC-12: Retry support (RFC 9000 §17.2.5) ----
    /// Token to include in Initial packets (received via Retry or NEW_TOKEN).
    retry_token: Vec<u8>,
    /// The original DCID sent in our first Initial (before any Retry).
    /// Needed for Retry integrity tag validation on the client side.
    original_dcid: ConnectionId,
    /// Whether a Retry has already been processed (at most one per attempt).
    retry_received: bool,
    /// Saved Initial CRYPTO data (ClientHello) for re-queuing after Retry.
    /// `write_hs()` drains its buffer, so we must cache it for the re-send.
    initial_crypto_data: Option<Vec<u8>>,
    /// Server-side secret used to mint NEW_TOKEN frames for future connections.
    new_token_secret: Option<[u8; 32]>,
    /// Whether a NEW_TOKEN frame has already been queued on this connection.
    new_token_sent: bool,
    /// Whether the peer's transport parameters have been parsed and applied.
    peer_transport_parameters_applied: bool,
    /// Peer's active connection ID limit.
    peer_active_connection_id_limit: u64,
    /// Peer's first non-Retry source connection ID, for transport-parameter validation.
    peer_initial_source_connection_id: Option<ConnectionId>,
    /// Retry source connection ID expected from server transport parameters.
    expected_retry_source_connection_id: Option<ConnectionId>,
    /// Pending validation state for a newly observed peer path.
    pending_peer_path_validation: Option<PendingPeerPathValidation>,
}

/// Wrapper for rustls QUIC connection (client or server).
pub(crate) enum QuicTls {
    Client(rustls::quic::ClientConnection),
    Server(rustls::quic::ServerConnection),
}

impl QuicTls {
    fn quic_transport_parameters(&self) -> Option<&[u8]> {
        match self {
            Self::Client(c) => c.quic_transport_parameters(),
            Self::Server(s) => s.quic_transport_parameters(),
        }
    }

    fn zero_rtt_keys(&self) -> Option<quic::DirectionalKeys> {
        match self {
            Self::Client(c) => c.zero_rtt_keys(),
            Self::Server(s) => s.zero_rtt_keys(),
        }
    }

    fn alpn_protocol(&self) -> Option<&[u8]> {
        match self {
            Self::Client(c) => c.alpn_protocol(),
            Self::Server(s) => s.alpn_protocol(),
        }
    }

    fn tls13_tickets_received(&self) -> u32 {
        match self {
            Self::Client(c) => c.tls13_tickets_received(),
            Self::Server(_) => 0,
        }
    }

    fn is_early_data_accepted(&self) -> Option<bool> {
        match self {
            Self::Client(c) => Some(c.is_early_data_accepted()),
            Self::Server(_) => None,
        }
    }
}

impl QuicConnection {
    fn transport_parameter_error(message: impl Into<String>) -> QuicError {
        QuicError::Transport(TransportErrorCode::TransportParameterError, message.into())
    }

    fn negotiated_idle_timeout(local_ms: u64, peer_ms: u64) -> Duration {
        let effective_ms = match (local_ms, peer_ms) {
            (0, 0) => 0,
            (0, peer) => peer,
            (local, 0) => local,
            (local, peer) => local.min(peer),
        };
        Duration::from_millis(effective_ms)
    }

    fn peer_path_state_for_addr(&self, peer_addr: SocketAddr) -> PeerPathState {
        if self.side == Side::Server && peer_addr == self.peer_addr {
            PeerPathState {
                bytes_received: self.bytes_received,
                bytes_sent: self.bytes_sent,
                validated: self.peer_address_validated,
            }
        } else {
            self.peer_paths.get(&peer_addr).copied().unwrap_or_default()
        }
    }

    fn store_active_peer_path_state(&mut self) {
        if self.side != Side::Server {
            return;
        }

        self.peer_paths.insert(
            self.peer_addr,
            PeerPathState {
                bytes_received: self.bytes_received,
                bytes_sent: self.bytes_sent,
                validated: self.peer_address_validated,
            },
        );
    }

    fn load_active_peer_path_state(&mut self) {
        if self.side != Side::Server {
            return;
        }

        let state = self
            .peer_paths
            .get(&self.peer_addr)
            .copied()
            .unwrap_or_default();
        self.bytes_received = state.bytes_received;
        self.bytes_sent = state.bytes_sent;
        self.peer_address_validated = state.validated;
    }

    fn set_active_peer_addr(&mut self, peer_addr: SocketAddr) {
        if self.side == Side::Server {
            self.store_active_peer_path_state();
            self.peer_paths.entry(peer_addr).or_default();
        }
        self.peer_addr = peer_addr;
        self.load_active_peer_path_state();
    }

    fn record_unvalidated_bytes_received(&mut self, peer_addr: SocketAddr, bytes: usize) {
        if self.side != Side::Server {
            return;
        }

        if peer_addr == self.peer_addr {
            if !self.peer_address_validated {
                self.bytes_received = self.bytes_received.saturating_add(bytes);
                self.store_active_peer_path_state();
            }
            return;
        }

        let state = self.peer_paths.entry(peer_addr).or_default();
        if !state.validated {
            state.bytes_received = state.bytes_received.saturating_add(bytes);
        }
    }

    fn record_unvalidated_bytes_sent(&mut self, peer_addr: SocketAddr, bytes: usize) {
        if self.side != Side::Server {
            return;
        }

        if peer_addr == self.peer_addr {
            if !self.peer_address_validated {
                self.bytes_sent = self.bytes_sent.saturating_add(bytes);
                self.store_active_peer_path_state();
            }
            return;
        }

        let state = self.peer_paths.entry(peer_addr).or_default();
        if !state.validated {
            state.bytes_sent = state.bytes_sent.saturating_add(bytes);
        }
    }

    fn mark_peer_path_validated(&mut self, peer_addr: SocketAddr) {
        if self.side != Side::Server {
            return;
        }

        if peer_addr == self.peer_addr {
            self.peer_address_validated = true;
            self.store_active_peer_path_state();
            return;
        }

        self.peer_paths.entry(peer_addr).or_default().validated = true;
    }

    fn should_auto_validate_peer_path(
        &self,
        space: PacketNumberSpace,
        peer_addr: SocketAddr,
    ) -> bool {
        self.side == Side::Server
            && space != PacketNumberSpace::Initial
            && peer_addr == self.peer_addr
    }

    fn apply_remembered_peer_transport_parameter_values(
        &mut self,
        params: &packet::PeerTransportParameters,
    ) {
        self.flow_control.set_send_max(params.initial_max_data);
        self.streams.apply_peer_initial_limits(
            params.initial_max_stream_data_bidi_local,
            params.initial_max_stream_data_bidi_remote,
            params.initial_max_stream_data_uni,
            params.initial_max_streams_bidi,
            params.initial_max_streams_uni,
        );
    }

    fn apply_peer_transport_parameter_values(&mut self, params: &packet::PeerTransportParameters) {
        self.flow_control.set_send_max(params.initial_max_data);
        self.streams.apply_peer_initial_limits(
            params.initial_max_stream_data_bidi_local,
            params.initial_max_stream_data_bidi_remote,
            params.initial_max_stream_data_uni,
            params.initial_max_streams_bidi,
            params.initial_max_streams_uni,
        );
        self.idle_timeout =
            Self::negotiated_idle_timeout(DEFAULT_IDLE_TIMEOUT_MS, params.max_idle_timeout_ms);
        self.peer_ack_delay_exponent = params.ack_delay_exponent;
        self.peer_max_ack_delay = Duration::from_millis(params.max_ack_delay_ms);
        self.loss_detection
            .set_max_ack_delay(self.peer_max_ack_delay);
        self.path_mtu.peer_max_udp_payload_size = params.max_udp_payload_size as usize;
        self.max_send_udp_payload_size = self
            .max_send_udp_payload_size
            .min(self.path_mtu.search_limit());
        if let Some(probe) = self.path_mtu.in_flight_probe {
            if probe.size > self.path_mtu.search_limit() {
                self.abandon_path_mtu_probe(probe.pn);
            }
        }
        if self.path_mtu.search_limit() > self.max_send_udp_payload_size {
            self.path_mtu.next_probe_at.get_or_insert_with(Instant::now);
        } else {
            self.path_mtu.next_probe_at = None;
        }
        self.peer_disable_active_migration = params.disable_active_migration;
        self.peer_active_connection_id_limit = params.active_connection_id_limit;
        if let Some(token) = params.stateless_reset_token {
            self.cid_manager
                .set_active_peer_stateless_reset_token(token);
        }
        self.peer_preferred_address = params
            .preferred_address
            .as_deref()
            .and_then(|encoded| self.select_preferred_peer_address(encoded));
        self.maybe_begin_preferred_address_validation();
    }

    fn select_preferred_peer_address(&self, encoded: &[u8]) -> Option<PreferredPeerAddress> {
        let decoded = packet::decode_preferred_address(encoded).ok()?;
        let peer_addr = if self.peer_addr.is_ipv4() {
            decoded.ipv4.or(decoded.ipv6)
        } else {
            decoded.ipv6.or(decoded.ipv4)
        }?;

        Some(PreferredPeerAddress {
            peer_addr,
            connection_id: decoded.connection_id,
            stateless_reset_token: decoded.stateless_reset_token,
        })
    }

    fn maybe_begin_preferred_address_validation(&mut self) {
        if self.side != Side::Client || self.state != ConnectionState::Connected {
            return;
        }

        let Some(preferred) = self.peer_preferred_address.clone() else {
            return;
        };

        if preferred.peer_addr == self.peer_addr {
            self.cid_manager
                .set_peer_cid(preferred.connection_id.clone());
            self.cid_manager
                .set_active_peer_stateless_reset_token(preferred.stateless_reset_token);
            self.peer_preferred_address = None;
            return;
        }

        self.start_path_validation(
            preferred.peer_addr,
            PathValidationKind::PreferredAddress(preferred),
        );
    }

    fn cache_peer_transport_parameters(&self, params: &packet::PeerTransportParameters) {
        let (Some(cache), Some(cache_key)) = (
            self.peer_transport_parameter_cache.as_ref(),
            self.peer_transport_parameter_cache_key.as_ref(),
        ) else {
            return;
        };

        if let Ok(mut cache) = cache.lock() {
            cache.insert(cache_key.clone(), params.clone());
        }
    }

    fn cache_new_token(&self, token: &[u8]) {
        let (Some(cache), Some(cache_key)) = (
            self.client_token_cache.as_ref(),
            self.peer_transport_parameter_cache_key.as_ref(),
        ) else {
            return;
        };

        if let Ok(mut cache) = cache.lock() {
            cache.insert(cache_key.clone(), token.to_vec());
        }
    }

    pub(crate) fn configure_client_resumption_state(
        &mut self,
        cache_key: String,
        cache: PeerTransportParameterCache,
        token_cache: ClientTokenCache,
    ) {
        self.peer_transport_parameter_cache_key = Some(cache_key.clone());
        self.peer_transport_parameter_cache = Some(cache.clone());
        self.client_token_cache = Some(token_cache.clone());

        let remembered = cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(&cache_key).cloned());
        if let Some(params) = remembered {
            self.remembered_peer_transport_parameters = Some(params.clone());
            self.apply_remembered_peer_transport_parameter_values(&params);
        }

        let remembered_token = token_cache
            .lock()
            .ok()
            .and_then(|mut cache| cache.remove(&cache_key));
        if let Some(token) = remembered_token {
            self.retry_token = token;
        }
    }

    fn refresh_zero_rtt_keys(&mut self) {
        if self.side == Side::Client && self.keys.one_rtt.is_some() {
            self.keys.discard_zero_rtt();
            return;
        }

        let has_zero_rtt = match self.side {
            Side::Client => self.keys.zero_rtt_local().is_some(),
            Side::Server => self.keys.zero_rtt_remote().is_some(),
        };
        if !has_zero_rtt {
            if let Some(keys) = self.tls.zero_rtt_keys() {
                if self.side == Side::Server || self.remembered_peer_transport_parameters.is_some()
                {
                    self.keys.set_zero_rtt(self.side == Side::Client, keys);
                }
            }
        }
    }

    fn requeue_zero_rtt_in_flight(&mut self) {
        let pns: Vec<u64> = self.zero_rtt_in_flight.iter().copied().collect();
        for pn in pns {
            if let Some(pkt) = self
                .loss_detection
                .remove_sent_packet(PacketNumberSpace::ApplicationData, pn)
            {
                self.congestion.on_packet_discarded(pkt.size);
            }
            if let Some(frames) =
                self.sent_frames[PacketNumberSpace::ApplicationData as usize].remove(&pn)
            {
                for frame in frames {
                    if frame.is_retransmittable() {
                        self.pending_frames
                            .push_back((PacketNumberSpace::ApplicationData, frame));
                    }
                }
            }
            self.zero_rtt_in_flight.remove(&pn);
        }
    }

    fn finalize_client_early_data_state(&mut self) {
        if self.side != Side::Client || self.keys.one_rtt.is_none() {
            return;
        }

        if !self.attempted_zero_rtt {
            self.keys.discard_zero_rtt();
            return;
        }

        if self.early_data_accepted.is_some() {
            self.keys.discard_zero_rtt();
            return;
        }

        let accepted = self.tls.is_early_data_accepted().unwrap_or(false);
        self.early_data_accepted = Some(accepted);
        if !accepted {
            self.requeue_zero_rtt_in_flight();
        }
        self.keys.discard_zero_rtt();
    }

    fn can_send_zero_rtt(&self) -> bool {
        self.side == Side::Client
            && self.keys.zero_rtt_local().is_some()
            && self.keys.one_rtt.is_none()
            && self.remembered_peer_transport_parameters.is_some()
    }

    fn application_send_kind(&self) -> Option<ApplicationPacketKind> {
        if self.keys.one_rtt.is_some() {
            Some(ApplicationPacketKind::OneRtt)
        } else if self.can_send_zero_rtt() {
            Some(ApplicationPacketKind::ZeroRtt)
        } else {
            None
        }
    }

    fn frame_allowed_in_zero_rtt(frame: &Frame) -> bool {
        !matches!(
            frame,
            Frame::Ack(_)
                | Frame::Crypto(_)
                | Frame::HandshakeDone
                | Frame::NewToken { .. }
                | Frame::PathResponse { .. }
                | Frame::RetireConnectionId { .. }
        )
    }

    fn apply_peer_transport_parameters(&mut self) -> Result<(), QuicError> {
        if self.peer_transport_parameters_applied {
            return Ok(());
        }

        let raw = self.tls.quic_transport_parameters();

        let Some(raw) = raw else {
            return Ok(());
        };

        if self.side == Side::Client && self.keys.one_rtt.is_none() {
            return Ok(());
        }

        let params = packet::parse_transport_parameters(raw)?;
        self.validate_peer_transport_parameters(&params)?;
        self.peer_transport_parameters_applied = true;
        self.apply_peer_transport_parameter_values(&params);
        if self.side == Side::Client {
            self.remembered_peer_transport_parameters = Some(params.clone());
            self.cache_peer_transport_parameters(&params);
        }

        Ok(())
    }

    fn validate_peer_transport_parameters(
        &self,
        params: &packet::PeerTransportParameters,
    ) -> Result<(), QuicError> {
        Self::validate_peer_transport_parameters_for_state(
            self.side,
            &self.original_dcid,
            self.peer_initial_source_connection_id.as_ref(),
            self.expected_retry_source_connection_id.as_ref(),
            params,
        )
    }

    fn validate_peer_transport_parameters_for_state(
        side: Side,
        original_dcid: &ConnectionId,
        peer_initial_source_connection_id: Option<&ConnectionId>,
        expected_retry_source_connection_id: Option<&ConnectionId>,
        params: &packet::PeerTransportParameters,
    ) -> Result<(), QuicError> {
        if params.ack_delay_exponent > packet::MAX_ACK_DELAY_EXPONENT {
            return Err(Self::transport_parameter_error(format!(
                "ack_delay_exponent {} exceeds {}",
                params.ack_delay_exponent,
                packet::MAX_ACK_DELAY_EXPONENT
            )));
        }

        let Some(expected_initial_scid) = peer_initial_source_connection_id else {
            return Err(Self::transport_parameter_error(
                "peer initial source connection ID is not known yet",
            ));
        };

        match params.initial_source_connection_id.as_ref() {
            Some(actual) if actual.as_slice() == expected_initial_scid.as_slice() => {}
            Some(actual) => {
                return Err(Self::transport_parameter_error(format!(
                    "initial_source_connection_id {:?} does not match peer source CID {:?}",
                    actual, expected_initial_scid
                )));
            }
            None => {
                return Err(Self::transport_parameter_error(
                    "missing initial_source_connection_id",
                ));
            }
        }

        match side {
            Side::Client => {
                match params.original_destination_connection_id.as_ref() {
                    Some(actual) if actual.as_slice() == original_dcid.as_slice() => {}
                    Some(actual) => {
                        return Err(Self::transport_parameter_error(format!(
                            "original_destination_connection_id {:?} does not match original DCID {:?}",
                            actual, original_dcid
                        )));
                    }
                    None => {
                        return Err(Self::transport_parameter_error(
                            "missing original_destination_connection_id",
                        ));
                    }
                }

                match (
                    expected_retry_source_connection_id,
                    params.retry_source_connection_id.as_ref(),
                ) {
                    (Some(expected), Some(actual)) if actual.as_slice() == expected.as_slice() => {}
                    (Some(expected), Some(actual)) => {
                        return Err(Self::transport_parameter_error(format!(
                            "retry_source_connection_id {:?} does not match Retry SCID {:?}",
                            actual, expected
                        )));
                    }
                    (Some(_), None) => {
                        return Err(Self::transport_parameter_error(
                            "missing retry_source_connection_id",
                        ));
                    }
                    (None, Some(actual)) => {
                        return Err(Self::transport_parameter_error(format!(
                            "unexpected retry_source_connection_id {:?}",
                            actual
                        )));
                    }
                    (None, None) => {}
                }
            }
            Side::Server => {
                if params.original_destination_connection_id.is_some() {
                    return Err(Self::transport_parameter_error(
                        "client transport parameters must not include original_destination_connection_id",
                    ));
                }
                if params.retry_source_connection_id.is_some() {
                    return Err(Self::transport_parameter_error(
                        "client transport parameters must not include retry_source_connection_id",
                    ));
                }
            }
        }

        Ok(())
    }

    fn validate_stream_final_size(
        stream_id: StreamId,
        stream: &StreamState,
        final_size: u64,
    ) -> Result<(), QuicError> {
        if let Some(existing) = stream.final_size {
            if existing != final_size {
                return Err(QuicError::Transport(
                    TransportErrorCode::FinalSizeError,
                    format!(
                        "stream {stream_id} changed final size from {existing} to {final_size}"
                    ),
                ));
            }
        }

        if stream.flow_control.received_max_offset() > final_size {
            return Err(QuicError::Transport(
                TransportErrorCode::FinalSizeError,
                format!("stream {stream_id} final size {final_size} is smaller than received data"),
            ));
        }

        Ok(())
    }

    /// Create a new client connection.
    pub(crate) fn new_client(
        socket: UdpSocket,
        peer_addr: SocketAddr,
        local_cid: ConnectionId,
        remote_cid: ConnectionId,
        tls: rustls::quic::ClientConnection,
    ) -> Result<Self, QuicError> {
        let original_dcid = remote_cid.clone();
        let provider = rustls::crypto::ring::default_provider();
        let initial_keys = keys::derive_initial_keys(remote_cid.as_slice(), true, &provider)?;
        let mut pkt_keys = PacketKeys::new();
        pkt_keys.set_from_quic_keys(PacketNumberSpace::Initial, initial_keys);

        let now = Instant::now();
        let mut peer_paths = HashMap::new();
        peer_paths.insert(
            peer_addr,
            PeerPathState {
                validated: true,
                ..PeerPathState::default()
            },
        );
        Ok(Self {
            state: ConnectionState::Initial,
            side: Side::Client,
            socket,
            peer_addr,
            tls: QuicTls::Client(tls),
            keys: pkt_keys,
            cid_manager: ConnectionIdManager::new(local_cid, remote_cid),
            streams: StreamManager::new(
                Side::Client,
                DEFAULT_INITIAL_MAX_STREAMS_BIDI,
                DEFAULT_INITIAL_MAX_STREAMS_UNI,
                DEFAULT_INITIAL_MAX_STREAM_DATA,
            ),
            flow_control: ConnectionFlowControl::new(
                DEFAULT_INITIAL_MAX_DATA,
                DEFAULT_INITIAL_MAX_DATA,
            ),
            loss_detection: LossDetection::new(),
            congestion: Box::new(Cubic::new()),
            next_pn: [0; 3],
            largest_recv_pn: [0; 3],
            largest_recv_pn_time: [None; 3],
            received_packets: [false; 3],
            pending_frames: VecDeque::new(),
            pending_targeted_frames: VecDeque::new(),
            ack_needed: [false; 3],
            ack_eliciting_packets_since_last_ack: [0; 3],
            ack_immediate: [false; 3],
            recv_pn_ranges: [vec![], vec![], vec![]],
            recv_ecn_counts: [EcnCounts::default(); 3],
            crypto_recv_buf: [BTreeMap::new(), BTreeMap::new(), BTreeMap::new()],
            crypto_recv_offset: [0; 3],
            crypto_send_offset: [0; 3],
            sent_frames: [BTreeMap::new(), BTreeMap::new(), BTreeMap::new()],
            zero_rtt_in_flight: BTreeSet::new(),
            last_ack_eliciting_recv: [None; 3],
            peer_ack_ecn_counts: [None; 3],
            sent_ecn_counts: [EcnCounts::default(); 3],
            outbound_ecn_enabled: true,
            peer_ack_delay_exponent: DEFAULT_ACK_DELAY_EXPONENT,
            peer_max_ack_delay: Duration::from_millis(packet::DEFAULT_MAX_ACK_DELAY_MS),
            local_ack_delay_exponent: DEFAULT_ACK_DELAY_EXPONENT,
            local_max_ack_delay: Duration::from_millis(packet::DEFAULT_MAX_ACK_DELAY_MS),
            max_send_udp_payload_size: MAX_DATAGRAM_SIZE,
            path_mtu: PathMtuState::new(),
            deferred_key_discards: Vec::new(),
            recv_buf: vec![0u8; MAX_LOCAL_UDP_PAYLOAD_SIZE],
            idle_timeout: Duration::from_millis(DEFAULT_IDLE_TIMEOUT_MS),
            last_recv_time: now,
            idle_timeout_start: now,
            sent_ack_eliciting_since_last_recv: false,
            last_send_time: now,
            next_paced_send_time: None,
            bytes_received: 0,
            bytes_sent: 0,
            peer_address_validated: true,
            peer_paths,
            draining_until: None,
            closing_frames: Vec::new(),
            closing_response_pending: false,
            remembered_peer_transport_parameters: None,
            peer_transport_parameter_cache: None,
            peer_transport_parameter_cache_key: None,
            client_token_cache: None,
            peer_disable_active_migration: false,
            peer_preferred_address: None,
            attempted_zero_rtt: false,
            early_data_accepted: None,
            endpoint_managed: false,
            incoming_datagrams: VecDeque::new(),
            next_secrets: None,
            local_key_phase: false,
            remote_key_phase: false,
            pending_remote_key_update: None,
            one_rtt_packets_sent_with_current_keys: 0,
            key_update_pn: 0,
            retry_token: Vec::new(),
            original_dcid,
            retry_received: false,
            initial_crypto_data: None,
            new_token_secret: None,
            new_token_sent: false,
            peer_transport_parameters_applied: false,
            peer_active_connection_id_limit: DEFAULT_ACTIVE_CONNECTION_ID_LIMIT,
            peer_initial_source_connection_id: None,
            expected_retry_source_connection_id: None,
            pending_peer_path_validation: None,
        })
    }

    /// Create a new server connection.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_server(
        socket: UdpSocket,
        peer_addr: SocketAddr,
        local_cid: ConnectionId,
        remote_cid: ConnectionId,
        tls: rustls::quic::ServerConnection,
        initial_dcid: &[u8],
        original_dcid: ConnectionId,
        retry_source_connection_id: Option<ConnectionId>,
        new_token_secret: Option<[u8; 32]>,
        peer_address_validated: bool,
    ) -> Result<Self, QuicError> {
        let provider = rustls::crypto::ring::default_provider();
        let initial_keys = keys::derive_initial_keys(initial_dcid, false, &provider)?;
        let mut pkt_keys = PacketKeys::new();
        pkt_keys.set_from_quic_keys(PacketNumberSpace::Initial, initial_keys);
        let peer_initial_source_connection_id = Some(remote_cid.clone());

        let now = Instant::now();
        let mut peer_paths = HashMap::new();
        peer_paths.insert(
            peer_addr,
            PeerPathState {
                validated: peer_address_validated,
                ..PeerPathState::default()
            },
        );
        Ok(Self {
            state: ConnectionState::Initial,
            side: Side::Server,
            socket,
            peer_addr,
            tls: QuicTls::Server(tls),
            keys: pkt_keys,
            cid_manager: ConnectionIdManager::new(local_cid, remote_cid),
            streams: StreamManager::new(
                Side::Server,
                DEFAULT_INITIAL_MAX_STREAMS_BIDI,
                DEFAULT_INITIAL_MAX_STREAMS_UNI,
                DEFAULT_INITIAL_MAX_STREAM_DATA,
            ),
            flow_control: ConnectionFlowControl::new(
                DEFAULT_INITIAL_MAX_DATA,
                DEFAULT_INITIAL_MAX_DATA,
            ),
            loss_detection: LossDetection::new(),
            congestion: Box::new(Cubic::new()),
            next_pn: [0; 3],
            largest_recv_pn: [0; 3],
            largest_recv_pn_time: [None; 3],
            received_packets: [false; 3],
            pending_frames: VecDeque::new(),
            pending_targeted_frames: VecDeque::new(),
            ack_needed: [false; 3],
            ack_eliciting_packets_since_last_ack: [0; 3],
            ack_immediate: [false; 3],
            recv_pn_ranges: [vec![], vec![], vec![]],
            recv_ecn_counts: [EcnCounts::default(); 3],
            crypto_recv_buf: [BTreeMap::new(), BTreeMap::new(), BTreeMap::new()],
            crypto_recv_offset: [0; 3],
            crypto_send_offset: [0; 3],
            sent_frames: [BTreeMap::new(), BTreeMap::new(), BTreeMap::new()],
            zero_rtt_in_flight: BTreeSet::new(),
            last_ack_eliciting_recv: [None; 3],
            peer_ack_ecn_counts: [None; 3],
            sent_ecn_counts: [EcnCounts::default(); 3],
            outbound_ecn_enabled: true,
            peer_ack_delay_exponent: DEFAULT_ACK_DELAY_EXPONENT,
            peer_max_ack_delay: Duration::from_millis(packet::DEFAULT_MAX_ACK_DELAY_MS),
            local_ack_delay_exponent: DEFAULT_ACK_DELAY_EXPONENT,
            local_max_ack_delay: Duration::from_millis(packet::DEFAULT_MAX_ACK_DELAY_MS),
            max_send_udp_payload_size: MAX_DATAGRAM_SIZE,
            path_mtu: PathMtuState::new(),
            deferred_key_discards: Vec::new(),
            recv_buf: vec![0u8; MAX_LOCAL_UDP_PAYLOAD_SIZE],
            idle_timeout: Duration::from_millis(DEFAULT_IDLE_TIMEOUT_MS),
            last_recv_time: now,
            idle_timeout_start: now,
            sent_ack_eliciting_since_last_recv: false,
            last_send_time: now,
            next_paced_send_time: None,
            bytes_received: 0,
            bytes_sent: 0,
            peer_address_validated,
            peer_paths,
            draining_until: None,
            closing_frames: Vec::new(),
            closing_response_pending: false,
            remembered_peer_transport_parameters: None,
            peer_transport_parameter_cache: None,
            peer_transport_parameter_cache_key: None,
            client_token_cache: None,
            peer_disable_active_migration: false,
            peer_preferred_address: None,
            attempted_zero_rtt: false,
            early_data_accepted: None,
            endpoint_managed: false,
            incoming_datagrams: VecDeque::new(),
            next_secrets: None,
            local_key_phase: false,
            remote_key_phase: false,
            pending_remote_key_update: None,
            one_rtt_packets_sent_with_current_keys: 0,
            key_update_pn: 0,
            retry_token: Vec::new(),
            original_dcid,
            retry_received: false,
            initial_crypto_data: None,
            new_token_secret,
            new_token_sent: false,
            peer_transport_parameters_applied: false,
            peer_active_connection_id_limit: DEFAULT_ACTIVE_CONNECTION_ID_LIMIT,
            peer_initial_source_connection_id,
            expected_retry_source_connection_id: retry_source_connection_id,
            pending_peer_path_validation: None,
        })
    }

    // ---- Public query API ----

    /// Get the current connection state.
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Whether the handshake is complete.
    pub fn is_established(&self) -> bool {
        self.state == ConnectionState::Connected
    }

    /// Get the negotiated ALPN protocol.
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.tls.alpn_protocol()
    }

    /// Whether this connection can currently send 0-RTT application data.
    pub fn can_send_early_data(&self) -> bool {
        self.can_send_zero_rtt()
    }

    /// Whether sent 0-RTT data was accepted by the peer, once known.
    pub fn early_data_accepted(&self) -> Option<bool> {
        self.early_data_accepted
    }

    /// Number of TLS 1.3 resumption tickets received on this connection.
    pub fn tls13_tickets_received(&self) -> u32 {
        self.tls.tls13_tickets_received()
    }

    /// Initiate a local 1-RTT key update (RFC 9001 §6).
    ///
    /// Returns `Ok(true)` when a new local key phase was installed and future
    /// 1-RTT packets will use it. Returns `Ok(false)` if there are no 1-RTT
    /// keys yet or a previously-initiated update is still waiting for the peer.
    pub fn initiate_key_update(&mut self) -> Result<bool, QuicError> {
        if self.keys.one_rtt.is_none() || self.next_secrets.is_none() {
            return Ok(false);
        }
        if self.pending_remote_key_update.is_some() {
            return Ok(false);
        }

        let next_keys = self
            .next_secrets
            .as_mut()
            .ok_or_else(|| {
                QuicError::Transport(
                    TransportErrorCode::KeyUpdateError,
                    "no next secrets available".into(),
                )
            })?
            .next_packet_keys();

        let target_phase = !self.local_key_phase;
        if let Some(ref mut keys) = self.keys.one_rtt {
            keys.local = next_keys.local;
        }
        self.pending_remote_key_update = Some(PendingRemoteKeyUpdate {
            key_phase: target_phase,
            remote: next_keys.remote,
        });
        self.local_key_phase = target_phase;
        self.one_rtt_packets_sent_with_current_keys = 0;

        Ok(true)
    }

    pub(crate) fn set_primary_local_stateless_reset_token(&mut self, token: [u8; 16]) {
        self.cid_manager
            .set_primary_local_stateless_reset_token(token);
    }

    pub(crate) fn local_cid_bytes(&self) -> Vec<Vec<u8>> {
        self.cid_manager
            .local_cids()
            .map(|entry| entry.cid.as_slice().to_vec())
            .collect()
    }

    pub(crate) fn local_cid_reset_tokens(&self) -> Vec<(Vec<u8>, [u8; 16])> {
        self.cid_manager
            .local_cids()
            .filter_map(|entry| {
                entry
                    .stateless_reset_token
                    .map(|token| (entry.cid.as_slice().to_vec(), token))
            })
            .collect()
    }

    pub(crate) fn encode_stateless_reset(
        token: [u8; 16],
        triggering_datagram_len: usize,
    ) -> Option<Vec<u8>> {
        if triggering_datagram_len <= MIN_STATELESS_RESET_LEN {
            return None;
        }

        use rand::Rng;

        let target_len = triggering_datagram_len
            .saturating_sub(1)
            .clamp(MIN_STATELESS_RESET_LEN, MAX_DATAGRAM_SIZE);
        let mut buf = vec![0u8; target_len];
        rand::rng().fill(&mut buf[..target_len - STATELESS_RESET_TOKEN_LEN]);
        buf[0] &= 0x7f;
        buf[0] |= 0x40;
        buf[target_len - STATELESS_RESET_TOKEN_LEN..].copy_from_slice(&token);
        Some(buf)
    }

    fn queue_targeted_frame(
        &mut self,
        peer_addr: SocketAddr,
        space: PacketNumberSpace,
        frame: Frame,
    ) {
        self.pending_targeted_frames
            .push_back((peer_addr, space, frame));
    }

    fn start_path_validation(&mut self, peer_addr: SocketAddr, kind: PathValidationKind) {
        if self.state != ConnectionState::Connected
            || peer_addr == self.peer_addr
            || self.cid_manager.cid_len() == 0
        {
            return;
        }

        if self.side == Side::Server {
            self.peer_paths.entry(peer_addr).or_default();
        }

        if matches!(&kind, PathValidationKind::ObservedPeerMigration)
            && self.side == Side::Client
            && self.peer_disable_active_migration
        {
            return;
        }

        if self
            .pending_peer_path_validation
            .as_ref()
            .is_some_and(|validation| validation.peer_addr == peer_addr && validation.kind == kind)
        {
            return;
        }
        if self.pending_peer_path_validation.is_some() {
            return;
        }

        use rand::Rng;

        let mut challenge_data = [0u8; 8];
        rand::rng().fill(&mut challenge_data);

        self.pending_peer_path_validation = Some(PendingPeerPathValidation {
            peer_addr,
            challenge_data,
            kind,
        });
        self.queue_targeted_frame(
            peer_addr,
            PacketNumberSpace::ApplicationData,
            Frame::PathChallenge {
                data: challenge_data,
            },
        );
    }

    fn begin_peer_path_validation(&mut self, peer_addr: SocketAddr) {
        self.start_path_validation(peer_addr, PathValidationKind::ObservedPeerMigration);
    }

    fn process_path_response(&mut self, peer_addr: SocketAddr, data: [u8; 8]) {
        let Some(validation) = self.pending_peer_path_validation.clone() else {
            return;
        };

        if validation.peer_addr == peer_addr && validation.challenge_data == data {
            self.mark_peer_path_validated(peer_addr);
            if peer_addr != self.peer_addr {
                self.reset_path_recovery_after_validation();
                match &validation.kind {
                    PathValidationKind::ObservedPeerMigration => {
                        self.cid_manager.rotate_active_peer_cid();
                    }
                    PathValidationKind::PreferredAddress(preferred) => {
                        self.cid_manager
                            .set_peer_cid(preferred.connection_id.clone());
                        self.cid_manager
                            .set_active_peer_stateless_reset_token(preferred.stateless_reset_token);
                        self.peer_preferred_address = None;
                    }
                }
            } else if let PathValidationKind::PreferredAddress(preferred) = &validation.kind {
                self.cid_manager
                    .set_peer_cid(preferred.connection_id.clone());
                self.cid_manager
                    .set_active_peer_stateless_reset_token(preferred.stateless_reset_token);
                self.peer_preferred_address = None;
            }
            self.set_active_peer_addr(peer_addr);
            self.pending_peer_path_validation = None;
            self.maybe_begin_preferred_address_validation();
        }
    }

    fn reset_path_recovery_after_validation(&mut self) {
        let peer_max_udp_payload_size = self.path_mtu.peer_max_udp_payload_size;

        for space in [
            PacketNumberSpace::Initial,
            PacketNumberSpace::Handshake,
            PacketNumberSpace::ApplicationData,
        ] {
            for pkt in self.loss_detection.take_sent_packets(space) {
                self.congestion.on_packet_discarded(pkt.size);
            }

            for frames in std::mem::take(&mut self.sent_frames[space as usize]).into_values() {
                for frame in frames {
                    if frame.is_retransmittable() {
                        self.pending_frames.push_back((space, frame));
                    }
                }
            }
        }

        self.loss_detection = LossDetection::new();
        self.loss_detection
            .set_max_ack_delay(self.peer_max_ack_delay);
        self.congestion = self.congestion.new_instance();
        self.next_paced_send_time = None;
        self.max_send_udp_payload_size = MAX_DATAGRAM_SIZE;
        self.path_mtu = PathMtuState::new();
        self.path_mtu.peer_max_udp_payload_size = peer_max_udp_payload_size;
        self.path_mtu.probe_upper_bound = self.path_mtu.search_limit();
        if self.path_mtu.search_limit() > self.max_send_udp_payload_size {
            self.path_mtu.next_probe_at = Some(Instant::now());
        }
    }

    fn peer_cid_for_addr(&self, peer_addr: SocketAddr) -> ConnectionId {
        if let Some(PendingPeerPathValidation {
            peer_addr: validation_addr,
            kind: PathValidationKind::PreferredAddress(preferred),
            ..
        }) = self.pending_peer_path_validation.as_ref()
        {
            if *validation_addr == peer_addr {
                return preferred.connection_id.clone();
            }
        }

        self.cid_manager.active_peer_cid().clone()
    }

    fn enter_draining(&mut self) {
        self.enter_draining_at(Instant::now());
    }

    fn enter_draining_at(&mut self, now: Instant) {
        if self.state == ConnectionState::Closed {
            return;
        }

        self.state = ConnectionState::Draining;
        self.draining_until = Some(now + self.loss_detection.pto_timeout() * 3);
        self.closing_frames.clear();
        self.closing_response_pending = false;
    }

    fn idle_timeout_active(&self) -> bool {
        !self.idle_timeout.is_zero()
            && matches!(
                self.state,
                ConnectionState::Initial | ConnectionState::Handshake | ConnectionState::Connected
            )
    }

    fn idle_deadline(&self) -> Option<Instant> {
        self.idle_timeout_active()
            .then_some(self.idle_timeout_start + self.idle_timeout)
    }

    fn keep_alive_deadline(&self) -> Option<Instant> {
        if self.state != ConnectionState::Connected || self.idle_timeout.is_zero() {
            return None;
        }
        Some(self.last_send_time + self.idle_timeout / KEEP_ALIVE_FRACTION)
    }

    fn record_packet_received(&mut self, now: Instant) {
        self.last_recv_time = now;
        self.idle_timeout_start = now;
        self.sent_ack_eliciting_since_last_recv = false;
    }

    fn record_packet_sent(&mut self, now: Instant, ack_eliciting: bool) {
        self.last_send_time = now;
        if ack_eliciting && !self.sent_ack_eliciting_since_last_recv {
            self.idle_timeout_start = now;
            self.sent_ack_eliciting_since_last_recv = true;
        }
    }

    fn close_deadline(&self) -> Option<Instant> {
        matches!(
            self.state,
            ConnectionState::Closing | ConnectionState::Draining
        )
        .then_some(self.draining_until)
        .flatten()
    }

    fn begin_local_close(&mut self, close_frames: Vec<(PacketNumberSpace, Frame)>, now: Instant) {
        self.state = ConnectionState::Closing;
        self.draining_until = Some(now + self.loss_detection.pto_timeout() * 3);
        self.closing_frames = close_frames;
        self.closing_response_pending = false;
        self.pending_frames.clear();
        self.pending_targeted_frames.clear();
        self.ack_needed = [false; 3];
        self.ack_eliciting_packets_since_last_ack = [0; 3];
        self.ack_immediate = [false; 3];
        self.path_mtu.in_flight_probe = None;
        self.path_mtu.next_probe_at = None;
    }

    fn local_close_frames(
        &self,
        error_code: u64,
        reason: &[u8],
    ) -> Vec<(PacketNumberSpace, Frame)> {
        let transport_close = Frame::ConnectionClose {
            is_transport: true,
            error_code: TransportErrorCode::ApplicationError.to_u64(),
            frame_type: Some(0),
            reason: Vec::new(),
        };
        let application_close = Frame::ConnectionClose {
            is_transport: false,
            error_code,
            frame_type: None,
            reason: reason.to_vec(),
        };

        let mut frames = Vec::new();
        if self.keys.one_rtt.is_some() {
            if self.keys.get(PacketNumberSpace::Handshake).is_some() {
                frames.push((PacketNumberSpace::Handshake, transport_close.clone()));
            }
            frames.push((PacketNumberSpace::ApplicationData, application_close));
            return frames;
        }

        if self.can_send_zero_rtt() {
            if self.keys.get(PacketNumberSpace::Initial).is_some() {
                frames.push((PacketNumberSpace::Initial, transport_close.clone()));
            }
            frames.push((PacketNumberSpace::ApplicationData, application_close));
            return frames;
        }

        if self.keys.get(PacketNumberSpace::Handshake).is_some() {
            frames.push((PacketNumberSpace::Handshake, transport_close));
        } else if self.keys.get(PacketNumberSpace::Initial).is_some() {
            frames.push((PacketNumberSpace::Initial, transport_close));
        }

        frames
    }

    fn can_send_to_addr(&self, peer_addr: SocketAddr, packet_len: usize) -> bool {
        if self.side != Side::Server {
            return true;
        }

        let state = self.peer_path_state_for_addr(peer_addr);
        if state.validated {
            return true;
        }

        // RFC 9000 caps unvalidated server sends at 3x the bytes received.
        // `saturating_mul` only reaches `usize::MAX` at absurd receive volumes,
        // but keeping it explicit avoids overflow in the comparison itself.
        state.bytes_sent.saturating_add(packet_len) <= state.bytes_received.saturating_mul(3)
    }

    fn ack_deadline(&self, space: PacketNumberSpace) -> Option<Instant> {
        let idx = space as usize;
        if !self.ack_needed[idx] || self.keys.get(space).is_none() {
            return None;
        }

        match space {
            PacketNumberSpace::Initial | PacketNumberSpace::Handshake => Some(Instant::now()),
            PacketNumberSpace::ApplicationData => {
                if self.ack_immediate[idx] || self.ack_eliciting_packets_since_last_ack[idx] >= 2 {
                    Some(Instant::now())
                } else {
                    self.last_ack_eliciting_recv[idx].map(|t| t + self.local_max_ack_delay)
                }
            }
        }
    }

    fn path_mtu_probe_interval(&self) -> Duration {
        self.loss_detection
            .rtt
            .smoothed_rtt
            .max(Duration::from_millis(10))
    }

    fn path_mtu_probe_timeout_deadline(&self) -> Option<Instant> {
        let probe = self.path_mtu.in_flight_probe?;
        Some(
            probe.sent_at
                + self
                    .loss_detection
                    .pto_timeout()
                    .max(self.path_mtu_probe_interval()),
        )
    }

    fn next_path_mtu_probe_size(&self) -> Option<usize> {
        let current = self.max_send_udp_payload_size;
        let limit = self.path_mtu.search_limit();
        if limit <= current {
            return None;
        }

        let doubled = current.saturating_mul(2);
        let candidate = if limit < doubled {
            current + (limit - current).div_ceil(2)
        } else {
            doubled
        };

        (candidate > current).then_some(candidate.min(limit))
    }

    fn schedule_next_path_mtu_probe(&mut self, now: Instant) {
        if self.next_path_mtu_probe_size().is_some() {
            self.path_mtu.next_probe_at = Some(now + self.path_mtu_probe_interval());
        } else {
            self.path_mtu.next_probe_at = None;
        }
    }

    fn complete_path_mtu_probe_success(&mut self, pn: u64, now: Instant) {
        let Some(probe) = self.path_mtu.in_flight_probe else {
            return;
        };
        if probe.pn != pn {
            return;
        }

        self.path_mtu.in_flight_probe = None;
        self.max_send_udp_payload_size = probe.size;
        self.schedule_next_path_mtu_probe(now);
    }

    fn abandon_path_mtu_probe(&mut self, pn: u64) {
        let Some(probe) = self.path_mtu.in_flight_probe else {
            return;
        };
        if probe.pn != pn {
            return;
        }

        self.path_mtu.in_flight_probe = None;
        self.sent_frames[PacketNumberSpace::ApplicationData as usize].remove(&pn);
        if let Some(pkt) = self
            .loss_detection
            .remove_sent_packet(PacketNumberSpace::ApplicationData, pn)
        {
            self.congestion.on_packet_discarded(pkt.size);
        }
    }

    fn complete_path_mtu_probe_failure(&mut self, pn: u64, now: Instant) {
        let Some(probe) = self.path_mtu.in_flight_probe else {
            return;
        };
        if probe.pn != pn {
            return;
        }

        self.path_mtu.in_flight_probe = None;
        self.sent_frames[PacketNumberSpace::ApplicationData as usize].remove(&pn);
        if let Some(pkt) = self
            .loss_detection
            .remove_sent_packet(PacketNumberSpace::ApplicationData, pn)
        {
            self.congestion.on_loss(pkt.size, pkt.time_sent, now);
        }
        self.path_mtu.probe_upper_bound = self
            .path_mtu
            .probe_upper_bound
            .min(probe.size.saturating_sub(1));
        self.schedule_next_path_mtu_probe(now);
    }

    fn handle_path_mtu_probe_timeout(&mut self, now: Instant) {
        let Some(deadline) = self.path_mtu_probe_timeout_deadline() else {
            return;
        };
        if now < deadline {
            return;
        }

        if let Some(probe) = self.path_mtu.in_flight_probe {
            self.complete_path_mtu_probe_failure(probe.pn, now);
        }
    }

    fn path_mtu_probe_due(&self, now: Instant) -> bool {
        self.state == ConnectionState::Connected
            && self.keys.one_rtt.is_some()
            && self.peer_transport_parameters_applied
            && self.path_mtu.in_flight_probe.is_none()
            && self.next_path_mtu_probe_size().is_some()
            && self
                .path_mtu
                .next_probe_at
                .is_none_or(|deadline| now >= deadline)
    }

    fn record_recv_ecn(&mut self, space: PacketNumberSpace, ecn: Option<UdpEcnCodepoint>) {
        let Some(ecn) = ecn else {
            return;
        };

        let counts = &mut self.recv_ecn_counts[space as usize];
        match ecn {
            UdpEcnCodepoint::Ect0 => counts.ect0 = counts.ect0.saturating_add(1),
            UdpEcnCodepoint::Ect1 => counts.ect1 = counts.ect1.saturating_add(1),
            UdpEcnCodepoint::Ce => counts.ce = counts.ce.saturating_add(1),
        }
    }

    fn should_ack_immediately_for_packet(
        had_previous_packets: bool,
        previous_largest: u64,
        full_pn: u64,
        range_count: usize,
    ) -> bool {
        range_count > 1
            || (had_previous_packets && full_pn < previous_largest)
            || (!had_previous_packets && full_pn > 0)
            || (had_previous_packets && full_pn > previous_largest + 1)
    }

    fn ack_ecn_counts(&self, space: PacketNumberSpace) -> Option<EcnCounts> {
        let counts = self.recv_ecn_counts[space as usize];
        (counts != EcnCounts::default()).then_some(counts)
    }

    fn disable_outbound_ecn(&mut self) {
        self.outbound_ecn_enabled = false;
    }

    fn acknowledge_stream_frame(&mut self, stream_frame: &StreamFrame) {
        if let Some(stream) = self.streams.get_mut(stream_frame.stream_id) {
            stream.acknowledge_sent_data(
                stream_frame.offset,
                stream_frame.data.len(),
                stream_frame.fin,
            );
            if stream.send == Some(SendState::DataSent) && stream.all_sent_data_acked() {
                stream.send = Some(SendState::DataRecvd);
            }
        }
    }

    /// Validate peer ACK_ECN counts before trusting them as a congestion signal
    /// (RFC 9000 §13.4.2). Counts must be monotonic and must not exceed either the
    /// cumulative ECT we actually sent in this space or the number of newly-acked
    /// ECN-capable packets. On any failure (including a missing ACK_ECN frame when
    /// ECN-marked packets were just acked), outbound ECN is disabled (RFC 9002 §7.1)
    /// instead of trusting the feedback. Returns the validated CE delta.
    fn validate_ack_ecn_and_count_ce(
        &mut self,
        space: PacketNumberSpace,
        ack_ecn: Option<EcnCounts>,
        acked: &[SentPacket],
        is_new_largest: bool,
    ) -> usize {
        if !self.outbound_ecn_enabled {
            return 0;
        }

        let idx = space as usize;
        let newly_acked_ect0 = acked
            .iter()
            .filter(|pkt| pkt.ecn_marking == Some(UdpEcnCodepoint::Ect0))
            .count() as u64;
        let newly_acked_ect1 = acked
            .iter()
            .filter(|pkt| pkt.ecn_marking == Some(UdpEcnCodepoint::Ect1))
            .count() as u64;
        let newly_acked_ecn = newly_acked_ect0 + newly_acked_ect1;
        let Some(ack_ecn) = ack_ecn else {
            if newly_acked_ecn > 0 && is_new_largest {
                self.disable_outbound_ecn();
            }
            return 0;
        };

        let previous = self.peer_ack_ecn_counts[idx].unwrap_or_default();
        let counts_regressed = ack_ecn.ect0 < previous.ect0
            || ack_ecn.ect1 < previous.ect1
            || ack_ecn.ce < previous.ce;
        if counts_regressed {
            if is_new_largest {
                self.disable_outbound_ecn();
            }
            return 0;
        }

        let ect0_delta = ack_ecn.ect0 - previous.ect0;
        let ect1_delta = ack_ecn.ect1 - previous.ect1;
        let ce_delta = ack_ecn.ce - previous.ce;
        let sent = self.sent_ecn_counts[idx];
        let exceeds_total_sent = ack_ecn.ect0 > sent.ect0 || ack_ecn.ect1 > sent.ect1;
        let exceeds_newly_acked = ect0_delta + ect1_delta + ce_delta > newly_acked_ecn;
        if exceeds_total_sent || exceeds_newly_acked {
            if is_new_largest {
                self.disable_outbound_ecn();
            }
            return 0;
        }

        self.peer_ack_ecn_counts[idx] = Some(ack_ecn);
        ce_delta as usize
    }

    pub(crate) fn handle_internal_timers(&mut self, now: Instant) {
        if self.idle_deadline().is_some_and(|deadline| now >= deadline) {
            self.state = ConnectionState::Closed;
            self.draining_until = None;
            self.closing_frames.clear();
            self.closing_response_pending = false;
            return;
        }

        if self
            .close_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            self.state = ConnectionState::Closed;
            self.draining_until = None;
            self.closing_frames.clear();
            self.closing_response_pending = false;
            return;
        }

        if self.state == ConnectionState::Connected
            && self
                .keep_alive_deadline()
                .is_some_and(|deadline| now >= deadline)
        {
            self.pending_frames
                .push_back((PacketNumberSpace::ApplicationData, Frame::Ping));
        }

        if matches!(
            self.state,
            ConnectionState::Closing | ConnectionState::Draining | ConnectionState::Closed
        ) {
            return;
        }

        self.handle_path_mtu_probe_timeout(now);
        self.queue_pto_probes_if_due(now);
    }

    pub(crate) fn next_internal_deadline(&self) -> Option<Instant> {
        if self.state == ConnectionState::Closed {
            return None;
        }
        if matches!(
            self.state,
            ConnectionState::Closing | ConnectionState::Draining
        ) {
            return self.close_deadline();
        }

        [
            self.loss_detection.pto_deadline(),
            self.path_mtu_probe_timeout_deadline(),
            self.path_mtu.next_probe_at,
            self.idle_deadline(),
            self.keep_alive_deadline(),
            self.ack_deadline(PacketNumberSpace::Initial),
            self.ack_deadline(PacketNumberSpace::Handshake),
            self.ack_deadline(PacketNumberSpace::ApplicationData),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(crate) fn queue_pto_probes_if_due(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.loss_detection.pto_deadline() else {
            return false;
        };
        if now < deadline {
            return false;
        }

        let mut queued = false;
        for space in [
            PacketNumberSpace::Initial,
            PacketNumberSpace::Handshake,
            PacketNumberSpace::ApplicationData,
        ] {
            if self.keys.get(space).is_some() && self.loss_detection.has_unacked(space) {
                self.pending_frames.push_back((space, Frame::Ping));
                queued = true;
            }
        }

        if queued {
            self.loss_detection.on_pto_timeout();
        }

        queued
    }

    fn has_pending_send_work(&self) -> bool {
        !self.pending_frames.is_empty()
            || !self.pending_targeted_frames.is_empty()
            || self.ack_needed.iter().any(|needed| *needed)
            || self.path_mtu_probe_due(Instant::now())
    }

    fn should_initiate_key_update(&self) -> bool {
        let Some(keys) = self.keys.one_rtt.as_ref() else {
            return false;
        };
        if self.pending_remote_key_update.is_some() {
            return false;
        }

        let limit = keys.local.confidentiality_limit();
        let threshold = limit.saturating_sub(KEY_UPDATE_SAFETY_MARGIN);
        self.one_rtt_packets_sent_with_current_keys >= threshold
    }

    fn maybe_initiate_key_update(&mut self) -> Result<(), QuicError> {
        if self.should_initiate_key_update() {
            let _ = self.initiate_key_update()?;
        }
        Ok(())
    }

    fn effective_recv_timeout(
        timeout: &Option<IOTimeout>,
        internal_deadline: Option<Instant>,
    ) -> Option<IOTimeout> {
        let user_deadline = timeout.as_ref().map(IOTimeout::cancel_at_time);
        let internal_deadline = internal_deadline.map(|deadline| {
            let now_instant = Instant::now();
            let now_system = SystemTime::now();
            now_system + deadline.saturating_duration_since(now_instant)
        });

        match (user_deadline, internal_deadline) {
            (Some(user), Some(internal)) => Some(IOTimeout::at_time(user.min(internal))),
            (Some(user), None) => Some(IOTimeout::at_time(user)),
            (None, Some(internal)) => Some(IOTimeout::at_time(internal)),
            (None, None) => None,
        }
    }

    // ---- Stream API ----

    /// Open a new bidirectional stream.
    pub fn open_bidi_stream(&mut self) -> Result<StreamId, QuicError> {
        self.streams.open_bidi()
    }

    /// Open a new unidirectional stream.
    pub fn open_uni_stream(&mut self) -> Result<StreamId, QuicError> {
        self.streams.open_uni()
    }

    /// Get a mutable reference to the stream manager.
    pub fn streams_mut(&mut self) -> &mut StreamManager {
        &mut self.streams
    }

    /// Find an incoming (remotely-initiated) bidirectional stream with data.
    pub fn accept_incoming_bidi(&self) -> Option<StreamId> {
        self.streams.find_incoming_bidi()
    }

    /// Find an incoming (remotely-initiated) unidirectional stream with data.
    pub fn accept_incoming_uni(&self) -> Option<StreamId> {
        self.streams.find_incoming_uni()
    }

    /// Collect incoming remotely-initiated unidirectional streams with buffered data.
    pub fn incoming_uni_stream_ids(&self) -> Vec<StreamId> {
        self.streams.incoming_uni_ids()
    }

    /// Write data to a stream. Returns the number of bytes written.
    pub fn stream_send(
        &mut self,
        stream_id: StreamId,
        data: &[u8],
        fin: bool,
    ) -> Result<usize, QuicError> {
        match self.application_send_kind() {
            Some(ApplicationPacketKind::ZeroRtt) => {}
            Some(ApplicationPacketKind::OneRtt) if self.state == ConnectionState::Connected => {}
            _ => {
                return Err(QuicError::Transport(
                    TransportErrorCode::StreamStateError,
                    format!(
                        "application data is not sendable while connection state is {:?}",
                        self.state
                    ),
                ));
            }
        }

        let stream = self.streams.get_mut(stream_id).ok_or_else(|| {
            QuicError::Transport(
                TransportErrorCode::StreamStateError,
                format!("unknown stream {stream_id}"),
            )
        })?;

        if stream.send.is_none() {
            return Err(QuicError::Transport(
                TransportErrorCode::StreamStateError,
                format!("stream {stream_id} is receive-only"),
            ));
        }

        // QUIC-8: Check send state allows sending
        if matches!(
            stream.send,
            Some(SendState::ResetSent | SendState::ResetRecvd | SendState::DataRecvd)
        ) {
            return Err(QuicError::Transport(
                TransportErrorCode::StreamStateError,
                format!("stream {stream_id} send side is closed"),
            ));
        }

        let send_window = stream
            .flow_control
            .send_window()
            .min(self.flow_control.send_window());
        let len = (data.len() as u64).min(send_window) as usize;

        // Only send FIN when all data fits; a short write means the caller
        // still has unsent bytes, so FIN must wait for a later call.
        let actual_fin = fin && len == data.len();

        if len > 0 || actual_fin {
            let offset = stream.send_offset;
            stream.send_offset += len as u64;
            stream.flow_control.on_data_sent(len as u64);
            self.flow_control.on_data_sent(len as u64);

            // QUIC-8: Transition send state (RFC 9000 §3.1)
            if stream.send == Some(SendState::Ready) {
                stream.send = Some(SendState::Send);
            }

            if actual_fin {
                stream.fin_sent = true;
                stream.send = Some(SendState::DataSent);
            }

            self.pending_frames.push_back((
                PacketNumberSpace::ApplicationData,
                Frame::Stream(StreamFrame {
                    stream_id,
                    offset,
                    fin: actual_fin,
                    data: data[..len].to_vec(),
                }),
            ));
        }

        Ok(len)
    }

    /// Read data from a stream's receive buffer. Returns bytes read.
    pub fn stream_recv(
        &mut self,
        stream_id: StreamId,
        buf: &mut [u8],
    ) -> Result<(usize, bool), QuicError> {
        let stream = self.streams.get_mut(stream_id).ok_or_else(|| {
            QuicError::Transport(
                TransportErrorCode::StreamStateError,
                format!("unknown stream {stream_id}"),
            )
        })?;

        if stream.recv.is_none() {
            return Err(QuicError::Transport(
                TransportErrorCode::StreamStateError,
                format!("stream {stream_id} is send-only"),
            ));
        }

        // QUIC-8: Check for reset
        if stream.recv == Some(RecvState::ResetRecvd) {
            let error_code = stream.reset_error_code.unwrap_or(0);
            stream.recv = Some(RecvState::ResetRead);
            return Err(QuicError::StreamReset(error_code));
        }

        let mut total = 0;
        let mut is_fin = false;

        while total < buf.len() {
            let offset = stream.recv_offset;
            if let Some(data) = stream.recv_buf.remove(&offset) {
                let copy_len = data.len().min(buf.len() - total);
                buf[total..total + copy_len].copy_from_slice(&data[..copy_len]);
                total += copy_len;
                stream.recv_offset += copy_len as u64;

                if copy_len < data.len() {
                    Self::buffer_stream_recv_data(
                        &mut stream.recv_buf,
                        stream.recv_offset,
                        data[copy_len..].to_vec(),
                    );
                }
            } else {
                break;
            }
        }

        if let Some(final_size) = stream.final_size {
            if stream.recv_offset >= final_size {
                is_fin = true;
                if stream.recv == Some(RecvState::SizeKnown) {
                    stream.recv = Some(RecvState::DataRecvd);
                }
                if stream.recv == Some(RecvState::DataRecvd) {
                    stream.recv = Some(RecvState::DataRead);
                }
            }
        }

        Ok((total, is_fin))
    }

    // ---- Connection driver ----

    /// Drive the connection — send and receive packets. Call this in a loop.
    pub async fn poll(&mut self) -> Result<(), QuicError> {
        self.poll_with_timeout(&None).await
    }

    /// Drive the connection forward with an optional I/O timeout.
    ///
    /// Identical to [`poll`](Self::poll), but bounds the socket recv wait
    /// to `timeout`. A `TimedOut` I/O error is returned as `QuicError::Io`
    /// if the timeout expires before any datagram arrives.
    pub async fn poll_with_timeout(
        &mut self,
        timeout: &Option<IOTimeout>,
    ) -> Result<(), QuicError> {
        if self.state == ConnectionState::Closed {
            return Err(QuicError::ConnectionDraining);
        }

        let now = Instant::now();
        if self.idle_deadline().is_some_and(|deadline| now >= deadline) {
            self.state = ConnectionState::Closed;
            self.draining_until = None;
            self.closing_frames.clear();
            self.closing_response_pending = false;
            return Err(QuicError::ConnectionClosed {
                error_code: TransportErrorCode::NoError,
                reason: "idle timeout".into(),
            });
        }

        if matches!(
            self.state,
            ConnectionState::Closing | ConnectionState::Draining
        ) {
            if self.state == ConnectionState::Closing && !self.incoming_datagrams.is_empty() {
                self.closing_response_pending = true;
            }
            self.incoming_datagrams.clear();
            if !self.endpoint_managed {
                let effective_timeout =
                    Self::effective_recv_timeout(timeout, self.next_internal_deadline());
                match self
                    .socket
                    .recv_from_with_timeout_and_ecn(&mut self.recv_buf, &effective_timeout)
                    .await
                {
                    Ok(_) => {
                        if self.state == ConnectionState::Closing {
                            self.closing_response_pending = true;
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(e) => return Err(QuicError::Io(e)),
                }
            }

            self.flush_pending_connection_close().await?;

            self.handle_internal_timers(Instant::now());
            if self.state == ConnectionState::Closed {
                return Err(QuicError::ConnectionDraining);
            }
            return Ok(());
        }

        // Process queued datagrams from endpoint (QUIC-6)
        while let Some(data) = self.incoming_datagrams.pop_front() {
            self.process_datagram(&data)?;
        }

        self.handle_internal_timers(Instant::now());
        if self.state == ConnectionState::Closed {
            return Err(QuicError::ConnectionClosed {
                error_code: TransportErrorCode::NoError,
                reason: "idle timeout".into(),
            });
        }
        if matches!(
            self.state,
            ConnectionState::Closing | ConnectionState::Draining
        ) {
            self.flush_pending_connection_close().await?;
            return Ok(());
        }

        // Flush any queued application, control, or probe frames before blocking.
        self.process_tls_output()?;
        if self.has_pending_send_work() {
            self.send_pending_frames().await?;
        }

        // In standalone mode, receive directly from socket
        if !self.endpoint_managed {
            let internal_deadline = self.next_internal_deadline();
            let effective_timeout = Self::effective_recv_timeout(timeout, internal_deadline);
            match self
                .socket
                .recv_from_with_timeout_and_ecn(&mut self.recv_buf, &effective_timeout)
                .await
            {
                Ok((len, addr, ecn)) => {
                    let datagram = self.recv_buf[..len].to_vec();
                    self.process_datagram_from_with_ecn(&datagram, addr, ecn)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    if internal_deadline.is_none_or(|deadline| Instant::now() < deadline) {
                        return Err(QuicError::Io(e));
                    }
                }
                Err(e) => return Err(QuicError::Io(e)),
            }
        }

        self.handle_internal_timers(Instant::now());
        if self.state == ConnectionState::Closed {
            return Err(QuicError::ConnectionDraining);
        }
        if matches!(
            self.state,
            ConnectionState::Closing | ConnectionState::Draining
        ) {
            self.flush_pending_connection_close().await?;
            return Ok(());
        }

        // Process TLS output (CRYPTO frames to send)
        self.process_tls_output()?;

        // Send pending frames (includes ACK generation)
        if self.has_pending_send_work() {
            self.send_pending_frames().await?;
        }

        // Periodically garbage-collect fully closed streams (QUIC-8)
        self.streams.gc_closed_streams();

        Ok(())
    }

    /// Feed a datagram from an external source (used by QuicEndpoint).
    pub fn feed_datagram(&mut self, data: &[u8]) -> Result<(), QuicError> {
        self.feed_datagram_from_with_ecn(data, self.peer_addr, None)
    }

    /// Feed a datagram and its source address from an external source.
    pub fn feed_datagram_from(
        &mut self,
        data: &[u8],
        peer_addr: SocketAddr,
    ) -> Result<(), QuicError> {
        self.feed_datagram_from_with_ecn(data, peer_addr, None)
    }

    pub(crate) fn feed_datagram_from_with_ecn(
        &mut self,
        data: &[u8],
        peer_addr: SocketAddr,
        ecn: Option<UdpEcnCodepoint>,
    ) -> Result<(), QuicError> {
        if self.state == ConnectionState::Closing {
            let _ = (data, peer_addr, ecn);
            self.closing_response_pending = true;
            return Ok(());
        }
        if matches!(
            self.state,
            ConnectionState::Draining | ConnectionState::Closed
        ) {
            return Ok(());
        }
        self.process_datagram_from_with_ecn(data, peer_addr, ecn)?;
        if !matches!(
            self.state,
            ConnectionState::Closing | ConnectionState::Draining | ConnectionState::Closed
        ) {
            self.process_tls_output()?;
        }
        Ok(())
    }

    /// Send any pending frames. Must be called after `feed_datagram()`.
    pub async fn flush(&mut self) -> Result<(), QuicError> {
        if matches!(
            self.state,
            ConnectionState::Closing | ConnectionState::Draining
        ) {
            self.flush_pending_connection_close().await?;
            return Ok(());
        }
        if matches!(
            self.state,
            ConnectionState::Draining | ConnectionState::Closed
        ) {
            return Ok(());
        }
        self.process_tls_output()?;
        self.send_pending_frames().await
    }

    /// Close the connection with an application error.
    pub async fn close(&mut self, error_code: u64, reason: &[u8]) -> Result<(), QuicError> {
        if matches!(
            self.state,
            ConnectionState::Closing | ConnectionState::Draining | ConnectionState::Closed
        ) {
            return Ok(());
        }

        let close_frames = self.local_close_frames(error_code, reason);
        if close_frames.is_empty() {
            return Err(QuicError::Transport(
                TransportErrorCode::InternalError,
                "no packet protection keys available to send CONNECTION_CLOSE".into(),
            ));
        }

        self.begin_local_close(close_frames, Instant::now());
        self.closing_response_pending = true;
        self.flush_pending_connection_close().await
    }

    // ---- Datagram processing (receive path) ----

    /// Check if a datagram is a stateless reset (RFC 9000 §10.3).
    ///
    /// A stateless reset looks like a short header packet whose last 16 bytes
    /// match a known peer stateless reset token.  The datagram must be at
    /// least 21 bytes (1 header + 4 random + 16 token) to be valid.
    fn is_stateless_reset(&self, data: &[u8]) -> bool {
        if data.len() < MIN_STATELESS_RESET_LEN {
            return false;
        }
        // Must have short header form (first bit = 0)
        if data[0] & 0x80 != 0 {
            return false;
        }
        let token_start = data.len() - 16;
        let token: [u8; 16] = data[token_start..].try_into().unwrap();
        self.cid_manager.is_peer_stateless_reset_token(&token)
    }

    fn should_silently_drop_protected_packet(err: &QuicError) -> bool {
        matches!(
            err,
            QuicError::InvalidPacket(msg)
                if matches!(
                    msg.as_str(),
                    "AEAD decryption failed" | "header protection decrypt failed"
                )
        )
    }

    fn trace_event(message: impl AsRef<str>) {
        if std::env::var_os("STARFISH_QUIC_TRACE").is_some() {
            eprintln!("starfish-quic: {}", message.as_ref());
        }
    }

    fn frame_kind(frame: &Frame) -> &'static str {
        match frame {
            Frame::Padding => "Padding",
            Frame::Ping => "Ping",
            Frame::Ack(_) => "Ack",
            Frame::Crypto(_) => "Crypto",
            Frame::NewToken { .. } => "NewToken",
            Frame::Stream(_) => "Stream",
            Frame::MaxData { .. } => "MaxData",
            Frame::MaxStreamData { .. } => "MaxStreamData",
            Frame::MaxStreams { .. } => "MaxStreams",
            Frame::DataBlocked { .. } => "DataBlocked",
            Frame::StreamDataBlocked { .. } => "StreamDataBlocked",
            Frame::StreamsBlocked { .. } => "StreamsBlocked",
            Frame::ResetStream { .. } => "ResetStream",
            Frame::ConnectionClose { .. } => "ConnectionClose",
            Frame::HandshakeDone => "HandshakeDone",
            Frame::PathChallenge { .. } => "PathChallenge",
            Frame::PathResponse { .. } => "PathResponse",
            Frame::NewConnectionId { .. } => "NewConnectionId",
            Frame::RetireConnectionId { .. } => "RetireConnectionId",
            Frame::StopSending { .. } => "StopSending",
        }
    }

    /// Process an incoming UDP datagram, handling coalesced packets.
    pub(crate) fn process_datagram(&mut self, data: &[u8]) -> Result<(), QuicError> {
        self.process_datagram_from_with_ecn(data, self.peer_addr, None)
    }

    fn process_datagram_from_with_ecn(
        &mut self,
        data: &[u8],
        peer_addr: SocketAddr,
        ecn: Option<UdpEcnCodepoint>,
    ) -> Result<(), QuicError> {
        self.record_unvalidated_bytes_received(peer_addr, data.len());

        let cid_len = self.cid_manager.cid_len() as usize;
        let mut remaining = data;

        while !remaining.is_empty() {
            let header = match packet::header::parse_header(remaining, cid_len) {
                Ok(h) => h,
                Err(e) => {
                    // If header parsing fails, check for stateless reset
                    // before propagating the error.
                    if self.is_stateless_reset(remaining) {
                        self.enter_draining();
                        return Ok(());
                    }
                    return Err(e);
                }
            };

            match header {
                PacketHeader::Long(ref h) => {
                    if h.packet_type != LongPacketType::Retry
                        && self.peer_initial_source_connection_id.is_none()
                    {
                        self.peer_initial_source_connection_id = Some(h.scid.clone());
                        if self.side == Side::Client {
                            // Once the client sees the server's first non-Retry SCID,
                            // it must use that CID for subsequent Handshake / 1-RTT packets.
                            self.cid_manager.set_peer_cid(h.scid.clone());
                        }
                    }

                    let space = match h.packet_type {
                        LongPacketType::Initial => PacketNumberSpace::Initial,
                        LongPacketType::Handshake => PacketNumberSpace::Handshake,
                        LongPacketType::ZeroRtt => PacketNumberSpace::ApplicationData,
                        LongPacketType::Retry => {
                            // QUIC-12: Client-side Retry handling (RFC 9000 §17.2.5)
                            self.process_retry(remaining, h)?;
                            return Ok(());
                        }
                    };

                    // Parse type-specific fields to find packet boundaries
                    let packet_end = match h.packet_type {
                        LongPacketType::Initial => {
                            let fields = crate::packet::initial::parse_initial_fields(
                                remaining,
                                h.header_len,
                                h.first_byte,
                            )?;
                            fields.pn_offset + fields.length as usize
                        }
                        LongPacketType::Handshake => {
                            let fields = crate::packet::handshake::parse_handshake_fields(
                                remaining,
                                h.header_len,
                                h.first_byte,
                            )?;
                            fields.pn_offset + fields.length as usize
                        }
                        LongPacketType::ZeroRtt => {
                            let fields = crate::packet::zero_rtt::parse_zero_rtt_fields(
                                remaining,
                                h.header_len,
                                h.first_byte,
                            )?;
                            fields.pn_offset + fields.length as usize
                        }
                        LongPacketType::Retry => remaining.len(),
                    };

                    let packet_end = packet_end.min(remaining.len());
                    if self.side == Side::Client && h.packet_type == LongPacketType::ZeroRtt {
                        remaining = &remaining[packet_end..];
                        continue;
                    }
                    let kind = match h.packet_type {
                        LongPacketType::Initial => ReceivedPacketKind::Initial,
                        LongPacketType::Handshake => ReceivedPacketKind::Handshake,
                        LongPacketType::ZeroRtt => ReceivedPacketKind::ZeroRtt,
                        LongPacketType::Retry => unreachable!(),
                    };
                    match self.process_packet(space, kind, &remaining[..packet_end], peer_addr, ecn)
                    {
                        Ok(()) => {
                            self.begin_peer_path_validation(peer_addr);
                        }
                        Err(e) if Self::should_silently_drop_protected_packet(&e) => {
                            Self::trace_event(format!(
                                "drop {:?}/{:?} len={} err={e}",
                                kind, space, packet_end
                            ));
                        }
                        Err(e) => return Err(e),
                    }
                    remaining = &remaining[packet_end..];
                }
                PacketHeader::Short(_) => {
                    // Short header packet extends to end of datagram.
                    // If processing fails (decrypt error or no keys), check
                    // for a stateless reset (RFC 9000 §10.3).
                    match self.process_packet(
                        PacketNumberSpace::ApplicationData,
                        ReceivedPacketKind::OneRtt,
                        remaining,
                        peer_addr,
                        ecn,
                    ) {
                        Ok(()) => {
                            self.begin_peer_path_validation(peer_addr);
                        }
                        Err(e) => {
                            if self.is_stateless_reset(remaining) {
                                self.enter_draining();
                                return Ok(());
                            }
                            if Self::should_silently_drop_protected_packet(&e) {
                                Self::trace_event(format!(
                                    "drop {:?}/{:?} len={} err={e}",
                                    ReceivedPacketKind::OneRtt,
                                    PacketNumberSpace::ApplicationData,
                                    remaining.len()
                                ));
                                break;
                            }
                            return Err(e);
                        }
                    }
                    // Also check for stateless reset when the packet was
                    // silently dropped (e.g. no keys yet). A legitimate
                    // 1-RTT packet would have been decrypted above.
                    if self.state != ConnectionState::Draining
                        && self.keys.get(PacketNumberSpace::ApplicationData).is_none()
                        && self.is_stateless_reset(remaining)
                    {
                        self.enter_draining();
                        return Ok(());
                    }
                    break;
                }
                PacketHeader::VersionNegotiation {
                    supported_versions, ..
                } => {
                    // QUIC-11: Version Negotiation handling (RFC 9000 §6.2)
                    // If our version is NOT supported, error out. If it IS
                    // supported, silently ignore (potential downgrade attack).
                    if self.side == Side::Client
                        && !supported_versions.contains(&packet::QUIC_VERSION_1)
                    {
                        return Err(QuicError::Transport(
                            TransportErrorCode::InternalError,
                            "server does not support QUIC v1".into(),
                        ));
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    /// Process a Retry packet received from the server (RFC 9000 §17.2.5).
    ///
    /// Validates the integrity tag, stores the token, resets Initial crypto
    /// state, and re-queues CRYPTO frames using the new DCID (Retry SCID).
    fn process_retry(
        &mut self,
        data: &[u8],
        header: &packet::header::LongHeader,
    ) -> Result<(), QuicError> {
        // Only clients process Retry packets
        if self.side != Side::Client {
            return Ok(());
        }

        // A client MUST accept and process at most one Retry packet (RFC 9000 §17.2.5.2)
        if self.retry_received {
            return Ok(());
        }

        // Validate the Retry Integrity Tag using the original DCID
        if !crate::packet::retry::verify_retry_integrity_tag(&self.original_dcid, data)? {
            return Err(QuicError::InvalidPacket(
                "retry integrity tag validation failed".into(),
            ));
        }

        // Parse the Retry fields to extract token and new SCID
        let retry = crate::packet::retry::parse_retry_fields(
            data,
            header.header_len,
            header.dcid.clone(),
            header.scid.clone(),
        )?;

        if retry.token.is_empty() {
            return Err(QuicError::InvalidPacket(
                "retry packet has empty token".into(),
            ));
        }

        // Store the token for inclusion in subsequent Initial packets
        self.retry_token = retry.token;
        self.retry_received = true;
        self.expected_retry_source_connection_id = Some(retry.scid.clone());

        // Update the remote CID to the Retry's SCID (RFC 9000 §17.2.5.1)
        self.cid_manager.set_peer_cid(retry.scid.clone());

        // Re-derive Initial keys using the new remote CID (Retry SCID)
        let provider = rustls::crypto::ring::default_provider();
        let new_initial_keys = keys::derive_initial_keys(retry.scid.as_slice(), true, &provider)?;
        self.keys
            .set_from_quic_keys(PacketNumberSpace::Initial, new_initial_keys);

        // Reset Initial packet number space
        self.next_pn[PacketNumberSpace::Initial as usize] = 0;
        self.largest_recv_pn[PacketNumberSpace::Initial as usize] = 0;
        self.largest_recv_pn_time[PacketNumberSpace::Initial as usize] = None;
        self.received_packets[PacketNumberSpace::Initial as usize] = false;
        self.recv_pn_ranges[PacketNumberSpace::Initial as usize].clear();
        self.recv_ecn_counts[PacketNumberSpace::Initial as usize] = EcnCounts::default();
        self.ack_needed[PacketNumberSpace::Initial as usize] = false;
        self.peer_ack_ecn_counts[PacketNumberSpace::Initial as usize] = None;
        self.sent_ecn_counts[PacketNumberSpace::Initial as usize] = EcnCounts::default();
        self.crypto_recv_offset[PacketNumberSpace::Initial as usize] = 0;
        self.crypto_send_offset[PacketNumberSpace::Initial as usize] = 0;
        self.crypto_recv_buf[PacketNumberSpace::Initial as usize].clear();
        self.sent_frames[PacketNumberSpace::Initial as usize].clear();
        for pkt in self
            .loss_detection
            .take_sent_packets(PacketNumberSpace::Initial)
        {
            self.congestion.on_packet_discarded(pkt.size);
        }

        // Remove any pending Initial frames (they used the old keys)
        self.pending_frames
            .retain(|(s, _)| *s != PacketNumberSpace::Initial);
        self.requeue_zero_rtt_in_flight();

        // Re-queue the cached ClientHello CRYPTO data with offset 0.
        // write_hs() drains its internal buffer, so calling process_tls_output()
        // again would produce no output. We must use the saved copy.
        if let Some(data) = self.initial_crypto_data.clone() {
            self.crypto_send_offset[PacketNumberSpace::Initial as usize] = data.len() as u64;
            self.pending_frames.push_back((
                PacketNumberSpace::Initial,
                Frame::Crypto(CryptoFrame { offset: 0, data }),
            ));
        }

        Ok(())
    }

    /// Process a single QUIC packet: remove protection, decrypt, parse frames.
    fn process_packet(
        &mut self,
        space: PacketNumberSpace,
        kind: ReceivedPacketKind,
        data: &[u8],
        peer_addr: SocketAddr,
        ecn: Option<UdpEcnCodepoint>,
    ) -> Result<(), QuicError> {
        // Make a mutable copy for in-place header protection removal and decryption
        let mut buf = data.to_vec();
        let cid_len = self.cid_manager.cid_len() as usize;

        let is_short_header = !packet::header::is_long_header(buf[0]);

        // Get keys for this space; drop packet if no keys available
        let remote_header_key = match kind {
            ReceivedPacketKind::ZeroRtt => match self.keys.zero_rtt_remote() {
                Some(k) => k.header.as_ref(),
                None => return Ok(()),
            },
            _ => match self.keys.get(space) {
                Some(k) => k.remote_header.as_ref(),
                None => return Ok(()),
            },
        };

        // Determine pn_offset based on packet type
        let (pn_offset, payload_end) = if !is_short_header {
            let header = packet::header::parse_header(&buf, cid_len)?;
            match header {
                PacketHeader::Long(h) => match h.packet_type {
                    LongPacketType::Initial => {
                        let fields = crate::packet::initial::parse_initial_fields(
                            &buf,
                            h.header_len,
                            h.first_byte,
                        )?;
                        (fields.pn_offset, fields.pn_offset + fields.length as usize)
                    }
                    LongPacketType::Handshake => {
                        let fields = crate::packet::handshake::parse_handshake_fields(
                            &buf,
                            h.header_len,
                            h.first_byte,
                        )?;
                        (fields.pn_offset, fields.pn_offset + fields.length as usize)
                    }
                    LongPacketType::ZeroRtt => {
                        let fields = crate::packet::zero_rtt::parse_zero_rtt_fields(
                            &buf,
                            h.header_len,
                            h.first_byte,
                        )?;
                        (fields.pn_offset, fields.pn_offset + fields.length as usize)
                    }
                    LongPacketType::Retry => (h.header_len, buf.len()),
                },
                _ => return Ok(()),
            }
        } else {
            let pn_offset = crate::packet::one_rtt::one_rtt_pn_offset(cid_len);
            (pn_offset, buf.len())
        };

        // Remove header protection (unmasks first byte and packet number bytes)
        let pn_len =
            header_protection::remove_header_protection(&mut buf, pn_offset, remote_header_key)?;

        // Read key phase from the now-unmasked first byte (must be after HP removal)
        let recv_key_phase = if is_short_header {
            buf[0] & 0x04 != 0
        } else {
            false
        };

        // Read truncated packet number
        let mut truncated_pn: u64 = 0;
        for i in 0..pn_len {
            truncated_pn = (truncated_pn << 8) | buf[pn_offset + i] as u64;
        }

        // Decode full packet number
        let full_pn = packet::decode_packet_number(
            truncated_pn,
            pn_len as u8,
            self.largest_recv_pn[space as usize],
        );

        // Split into header (AAD) and ciphertext
        let header_end = pn_offset + pn_len;
        let payload_end = payload_end.min(buf.len());

        if header_end >= payload_end {
            return Err(QuicError::InvalidPacket(
                "packet too short for payload".into(),
            ));
        }

        let header_bytes = buf[..header_end].to_vec();
        let mut ciphertext = buf[header_end..payload_end].to_vec();

        // AEAD decrypt — try key rotation if this is a 1-RTT packet with a different key phase
        let plaintext_len = if is_short_header && recv_key_phase != self.remote_key_phase {
            // Try to decrypt with next keys (key update in progress)
            self.try_decrypt_with_next_keys(
                full_pn,
                recv_key_phase,
                &header_bytes,
                &mut ciphertext,
            )?
        } else {
            // Normal decryption with current keys
            let remote_packet_key = match kind {
                ReceivedPacketKind::ZeroRtt => match self.keys.zero_rtt_remote() {
                    Some(k) => k.packet.as_ref(),
                    None => return Ok(()),
                },
                _ => match self.keys.get(space) {
                    Some(k) => k.remote.as_ref(),
                    None => return Ok(()),
                },
            };
            aead::decrypt_packet(full_pn, &header_bytes, &mut ciphertext, remote_packet_key)?
        };

        // Parse frames from decrypted payload
        let frames = frame::decode_frames(&ciphertext[..plaintext_len])?;
        let frame_kinds: Vec<&'static str> = frames.iter().map(Self::frame_kind).collect();
        Self::trace_event(format!(
            "recv {:?}/{:?} pn={} len={} frames={:?}",
            kind,
            space,
            full_pn,
            data.len(),
            frame_kinds
        ));

        // Update idle timeout timer (QUIC-5)
        let recv_time = Instant::now();
        self.record_packet_received(recv_time);

        // Update largest received PN
        let space_idx = space as usize;
        let had_previous_packets = self.received_packets[space_idx];
        let previous_largest = self.largest_recv_pn[space_idx];
        if !had_previous_packets || full_pn > previous_largest {
            self.largest_recv_pn[space_idx] = full_pn;
            self.largest_recv_pn_time[space_idx] = Some(recv_time);
        }
        self.received_packets[space_idx] = true;

        // Record received PN for ACK generation
        self.record_recv_pn(space, full_pn);
        self.record_recv_ecn(space, ecn);

        // Check if any frame is ack-eliciting
        let has_ack_eliciting = frames.iter().any(|f| f.is_ack_eliciting());
        if has_ack_eliciting {
            self.ack_needed[space_idx] = true;
            self.last_ack_eliciting_recv[space_idx] = Some(recv_time);
            self.ack_eliciting_packets_since_last_ack[space_idx] =
                self.ack_eliciting_packets_since_last_ack[space_idx].saturating_add(1);
            if Self::should_ack_immediately_for_packet(
                had_previous_packets,
                previous_largest,
                full_pn,
                self.recv_pn_ranges[space_idx].len(),
            ) {
                self.ack_immediate[space_idx] = true;
            }
        }

        // Process each frame
        for f in frames {
            self.process_frame_from(space, kind, f, peer_addr)?;
        }

        if self.should_auto_validate_peer_path(space, peer_addr) {
            self.mark_peer_path_validated(peer_addr);
        }

        Ok(())
    }

    // ---- Frame dispatch ----

    /// Process a single decoded frame.
    #[cfg(test)]
    fn process_frame(&mut self, space: PacketNumberSpace, frame: Frame) -> Result<(), QuicError> {
        let kind = match space {
            PacketNumberSpace::Initial => ReceivedPacketKind::Initial,
            PacketNumberSpace::Handshake => ReceivedPacketKind::Handshake,
            PacketNumberSpace::ApplicationData => ReceivedPacketKind::OneRtt,
        };
        self.process_frame_from(space, kind, frame, self.peer_addr)
    }

    fn process_frame_from(
        &mut self,
        space: PacketNumberSpace,
        kind: ReceivedPacketKind,
        frame: Frame,
        peer_addr: SocketAddr,
    ) -> Result<(), QuicError> {
        if kind == ReceivedPacketKind::ZeroRtt && !Self::frame_allowed_in_zero_rtt(&frame) {
            return Err(QuicError::Transport(
                TransportErrorCode::ProtocolViolation,
                format!("frame {:?} is not permitted in 0-RTT packets", frame),
            ));
        }

        match frame {
            Frame::Padding | Frame::Ping => {
                // No-op (ACK already scheduled if ack-eliciting)
            }

            Frame::Ack(ack) => {
                let ack_ranges: Vec<(u64, u64)> =
                    ack.ranges.iter().map(|r| (r.start, r.end)).collect();
                let previous_largest_acked = self.loss_detection.largest_acked(space);
                // Scale ACK delay by 2^ack_delay_exponent (RFC 9000 §19.3).
                // Clamp the exponent to RFC 9000's maximum even if the internal
                // state was corrupted or loaded from stale cached data.
                let ack_delay_us = ack
                    .ack_delay
                    .checked_shl(
                        self.peer_ack_delay_exponent
                            .min(packet::MAX_ACK_DELAY_EXPONENT),
                    )
                    .unwrap_or(u64::MAX);
                let ack_delay = Duration::from_micros(ack_delay_us);
                let now = Instant::now();

                let (acked, lost) = self.loss_detection.on_ack_received(
                    space,
                    ack.largest_acknowledged,
                    ack_delay,
                    &ack_ranges,
                    now,
                );
                let persistent_congestion = self
                    .loss_detection
                    .detects_persistent_congestion(space, &acked, &lost);
                self.loss_detection
                    .prune_acknowledged_ack_eliciting_times(space);

                let rtt = self.loss_detection.rtt.latest_rtt;
                let is_new_largest = previous_largest_acked
                    .is_none_or(|previous| ack.largest_acknowledged > previous);
                let mut ce_marked_packets =
                    self.validate_ack_ecn_and_count_ce(space, ack.ecn, &acked, is_new_largest);
                let space_idx = space as usize;
                for pkt in &acked {
                    self.zero_rtt_in_flight.remove(&pkt.pn);
                    if pkt.space == PacketNumberSpace::ApplicationData {
                        self.complete_path_mtu_probe_success(pkt.pn, now);
                    }
                    if ce_marked_packets > 0
                        && matches!(
                            pkt.ecn_marking,
                            Some(UdpEcnCodepoint::Ect0 | UdpEcnCodepoint::Ect1)
                        )
                    {
                        self.congestion.on_ecn_ce(pkt.size, pkt.time_sent, now);
                        ce_marked_packets -= 1;
                    } else {
                        // Pass sent_time for recovery check, now for epoch timing (RFC 9002 §7.3.2)
                        self.congestion.on_ack(pkt.size, rtt, pkt.time_sent, now);
                    }
                    if let Some(frames) = self.sent_frames[space_idx].remove(&pkt.pn) {
                        for f in &frames {
                            if let Frame::Stream(sf) = f {
                                self.acknowledge_stream_frame(sf);
                            }
                        }
                    }
                }
                for pkt in &lost {
                    self.zero_rtt_in_flight.remove(&pkt.pn);
                    if pkt.space == PacketNumberSpace::ApplicationData {
                        self.complete_path_mtu_probe_failure(pkt.pn, now);
                    }
                    self.congestion.on_loss(pkt.size, pkt.time_sent, now);
                    // Re-queue retransmittable frames from lost packets
                    if let Some(frames) = self.sent_frames[space_idx].remove(&pkt.pn) {
                        for f in frames {
                            if f.is_retransmittable() {
                                self.pending_frames.push_back((space, f));
                            }
                        }
                    }
                }
                if persistent_congestion {
                    self.congestion.on_persistent_congestion();
                }
            }

            Frame::Crypto(crypto_frame) => {
                Self::trace_event(format!(
                    "crypto {:?} offset={} len={}",
                    space,
                    crypto_frame.offset,
                    crypto_frame.data.len()
                ));
                self.process_crypto_data(space, crypto_frame.offset, crypto_frame.data)?;
            }

            Frame::Stream(stream_frame) => {
                let sid = stream_frame.stream_id;

                // Accept the stream if it's remotely-initiated and new
                if self.streams.get(sid).is_none() {
                    self.streams.accept_stream(sid)?;
                }

                if let Some(stream) = self.streams.get_mut(sid) {
                    if stream.recv.is_none() {
                        return Err(QuicError::Transport(
                            TransportErrorCode::StreamStateError,
                            format!("stream {sid} is send-only"),
                        ));
                    }

                    let data_len = stream_frame.data.len() as u64;
                    let end = stream_frame.offset.checked_add(data_len).ok_or_else(|| {
                        QuicError::Transport(
                            TransportErrorCode::FinalSizeError,
                            format!("stream {sid} final size overflow"),
                        )
                    })?;

                    if let Some(final_size) = stream.final_size {
                        if end > final_size {
                            return Err(QuicError::Transport(
                                TransportErrorCode::FinalSizeError,
                                format!(
                                    "stream {sid} received data beyond final size {final_size}"
                                ),
                            ));
                        }
                    }

                    // Track the highest byte offset (RFC 9000 §4.1).  The
                    // delta is the new bytes not yet counted, so overlapping
                    // retransmissions don't inflate the connection total.
                    let delta = stream
                        .flow_control
                        .on_data_received(stream_frame.offset, data_len);
                    self.flow_control.on_data_received(delta);

                    // Enforce flow control limits (RFC 9000 §4.1)
                    if !stream.flow_control.check_recv_limit() {
                        return Err(QuicError::Transport(
                            TransportErrorCode::FlowControlError,
                            format!("stream {sid} exceeded stream-level flow control"),
                        ));
                    }
                    if !self.flow_control.check_recv_limit() {
                        return Err(QuicError::Transport(
                            TransportErrorCode::FlowControlError,
                            "connection-level flow control exceeded".into(),
                        ));
                    }

                    if !stream_frame.data.is_empty() {
                        Self::buffer_stream_recv_data(
                            &mut stream.recv_buf,
                            stream_frame.offset,
                            stream_frame.data,
                        );
                    }

                    if stream_frame.fin {
                        Self::validate_stream_final_size(sid, stream, end)?;
                        stream.final_size = Some(end);
                        // QUIC-8: Transition recv state to SizeKnown (RFC 9000 ?3.2)
                        if stream.recv == Some(RecvState::Recv) {
                            stream.recv = Some(RecvState::SizeKnown);
                        }
                    }
                    if stream.recv == Some(RecvState::SizeKnown)
                        && Self::stream_has_received_all_data(stream)
                    {
                        stream.recv = Some(RecvState::DataRecvd);
                    }
                }
            }

            Frame::MaxData { maximum_data } => {
                self.flow_control.update_send_max(maximum_data);
            }

            Frame::MaxStreamData {
                stream_id,
                maximum_stream_data,
            } => {
                if let Some(stream) = self.streams.get_mut(stream_id) {
                    stream.flow_control.update_send_max(maximum_stream_data);
                }
            }

            Frame::MaxStreams {
                bidirectional,
                maximum_streams,
            } => {
                if maximum_streams > MAX_STREAMS_LIMIT {
                    return Err(QuicError::Transport(
                        TransportErrorCode::FrameEncodingError,
                        format!("MAX_STREAMS exceeds protocol limit: {maximum_streams}"),
                    ));
                }
                if bidirectional {
                    self.streams.update_max_bidi_streams(maximum_streams);
                } else {
                    self.streams.update_max_uni_streams(maximum_streams);
                }
            }

            Frame::ResetStream {
                stream_id,
                application_error_code,
                final_size,
            } => {
                if self.streams.get(stream_id).is_none() {
                    self.streams.accept_stream(stream_id)?;
                }
                if let Some(stream) = self.streams.get_mut(stream_id) {
                    if stream.recv.is_none() {
                        return Err(QuicError::Transport(
                            TransportErrorCode::StreamStateError,
                            format!("stream {stream_id} is send-only"),
                        ));
                    }

                    Self::validate_stream_final_size(stream_id, stream, final_size)?;
                    stream.final_size = Some(final_size);
                    stream.reset_error_code = Some(application_error_code);
                    stream.recv_buf.clear();
                    // QUIC-8: Transition recv state to ResetRecvd (RFC 9000 §3.2)
                    stream.recv = Some(RecvState::ResetRecvd);
                }
            }

            Frame::ConnectionClose {
                error_code, reason, ..
            } => {
                self.enter_draining();
                // Log or store the close reason for diagnostic purposes
                let _ = (error_code, reason);
            }

            Frame::HandshakeDone => {
                // Client receives this from server
                if self.side == Side::Client {
                    self.state = ConnectionState::Connected;
                    self.finalize_client_early_data_state();
                    // Discard Handshake keys (RFC 9001 §4.9.2)
                    self.keys.discard(PacketNumberSpace::Handshake);
                    for pkt in self
                        .loss_detection
                        .take_sent_packets(PacketNumberSpace::Handshake)
                    {
                        self.congestion.on_packet_discarded(pkt.size);
                    }
                    self.sent_frames[PacketNumberSpace::Handshake as usize].clear();
                    self.recv_ecn_counts[PacketNumberSpace::Handshake as usize] =
                        EcnCounts::default();
                    self.peer_ack_ecn_counts[PacketNumberSpace::Handshake as usize] = None;
                    self.sent_ecn_counts[PacketNumberSpace::Handshake as usize] =
                        EcnCounts::default();
                }
            }

            Frame::PathChallenge { data } => {
                // Respond with PATH_RESPONSE
                self.queue_targeted_frame(
                    peer_addr,
                    PacketNumberSpace::ApplicationData,
                    Frame::PathResponse { data },
                );
            }

            Frame::NewConnectionId {
                sequence_number,
                retire_prior_to,
                connection_id,
                stateless_reset_token,
            } => {
                if retire_prior_to > sequence_number {
                    return Err(QuicError::Transport(
                        TransportErrorCode::FrameEncodingError,
                        format!(
                            "NEW_CONNECTION_ID retire_prior_to {retire_prior_to} exceeds sequence {sequence_number}"
                        ),
                    ));
                }
                if !(1..=packet::MAX_CID_LEN).contains(&connection_id.len()) {
                    return Err(QuicError::Transport(
                        TransportErrorCode::FrameEncodingError,
                        format!(
                            "NEW_CONNECTION_ID invalid connection ID length {}",
                            connection_id.len()
                        ),
                    ));
                }
                let cid = ConnectionId::from_slice(&connection_id);
                self.cid_manager.add_peer_cid(
                    sequence_number,
                    cid,
                    stateless_reset_token,
                    retire_prior_to,
                )?;
            }

            Frame::RetireConnectionId { sequence_number } => {
                self.cid_manager.retire_local_cid(sequence_number);
            }
            Frame::StopSending {
                stream_id,
                application_error_code,
            } => {
                // Peer requests we stop sending on this stream (RFC 9000 ?3.3).
                // Peer-initiated streams can be created implicitly by reordered
                // STOP_SENDING or RESET_STREAM before the first STREAM frame arrives.
                if self.streams.get(stream_id).is_none() {
                    self.streams.accept_stream(stream_id)?;
                }
                let Some(stream) = self.streams.get_mut(stream_id) else {
                    return Err(QuicError::Transport(
                        TransportErrorCode::StreamStateError,
                        format!("unknown stream {stream_id}"),
                    ));
                };
                if stream.send.is_none() {
                    return Err(QuicError::Transport(
                        TransportErrorCode::StreamStateError,
                        format!("stream {stream_id} is receive-only"),
                    ));
                }
                let final_size = stream.send_offset;
                // QUIC-8: Transition send state to ResetSent (RFC 9000 ?3.1)
                stream.send = Some(SendState::ResetSent);
                self.pending_frames.push_back((
                    PacketNumberSpace::ApplicationData,
                    Frame::ResetStream {
                        stream_id,
                        application_error_code,
                        final_size,
                    },
                ));
            }

            Frame::NewToken { token } => {
                if self.side != Side::Client {
                    return Err(QuicError::Transport(
                        TransportErrorCode::ProtocolViolation,
                        "clients must not send NEW_TOKEN".into(),
                    ));
                }
                if token.is_empty() {
                    return Err(QuicError::Transport(
                        TransportErrorCode::FrameEncodingError,
                        "NEW_TOKEN token must be non-empty".into(),
                    ));
                }
                if self.retry_token != token {
                    self.retry_token = token.clone();
                    self.cache_new_token(&token);
                }
            }

            // Frames we acknowledge but don't need to act on further
            Frame::DataBlocked { maximum_data } => {
                if let Some(new_max) = self.flow_control.update_from_data_blocked(maximum_data) {
                    self.pending_frames.push_back((
                        PacketNumberSpace::ApplicationData,
                        Frame::MaxData {
                            maximum_data: new_max,
                        },
                    ));
                }
            }

            Frame::StreamDataBlocked {
                stream_id,
                maximum_stream_data,
            } => {
                if let Some(stream) = self.streams.get_mut(stream_id) {
                    if stream.recv.is_some() {
                        if let Some(new_max) = stream
                            .flow_control
                            .update_from_stream_data_blocked(maximum_stream_data)
                        {
                            self.pending_frames.push_back((
                                PacketNumberSpace::ApplicationData,
                                Frame::MaxStreamData {
                                    stream_id,
                                    maximum_stream_data: new_max,
                                },
                            ));
                        }
                    }
                }
            }

            Frame::StreamsBlocked {
                bidirectional,
                maximum_streams,
            } => {
                let current_limit = self.streams.local_max_streams(bidirectional);
                if maximum_streams < current_limit {
                    self.pending_frames.push_back((
                        PacketNumberSpace::ApplicationData,
                        Frame::MaxStreams {
                            bidirectional,
                            maximum_streams: current_limit,
                        },
                    ));
                }
            }

            Frame::PathResponse { data } => {
                self.process_path_response(peer_addr, data);
            }
        }

        Ok(())
    }

    // ---- CRYPTO stream processing ----

    /// Process received CRYPTO frame data, feeding it to rustls.
    fn process_crypto_data(
        &mut self,
        space: PacketNumberSpace,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<(), QuicError> {
        let idx = space as usize;
        let expected = self.crypto_recv_offset[idx];

        if offset + data.len() as u64 <= expected {
            // Duplicate data, ignore
            return Ok(());
        }

        if offset > expected {
            // Out of order — buffer for later reassembly
            match self.crypto_recv_buf[idx].entry(offset) {
                std::collections::btree_map::Entry::Occupied(mut existing) => {
                    if existing.get().len() < data.len() {
                        existing.insert(data);
                    }
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(data);
                }
            }
            return Ok(());
        }

        // In-order: trim any overlap and feed to TLS
        let skip = (expected - offset) as usize;
        let useful = &data[skip..];
        self.crypto_recv_offset[idx] += useful.len() as u64;

        match &mut self.tls {
            QuicTls::Client(c) => c.read_hs(useful).map_err(QuicError::Tls)?,
            QuicTls::Server(s) => s.read_hs(useful).map_err(QuicError::Tls)?,
        }

        // Check if buffered data is now contiguous
        loop {
            let next_expected = self.crypto_recv_offset[idx];
            let stale: Vec<u64> = self.crypto_recv_buf[idx]
                .iter()
                .filter_map(|(&start, chunk)| {
                    (start + chunk.len() as u64 <= next_expected).then_some(start)
                })
                .collect();
            for start in stale {
                self.crypto_recv_buf[idx].remove(&start);
            }

            let next_start = self.crypto_recv_buf[idx]
                .range(..=next_expected)
                .rev()
                .find_map(|(&start, chunk)| {
                    (start + chunk.len() as u64 > next_expected).then_some(start)
                });

            let Some(start) = next_start else {
                break;
            };

            let buffered = self.crypto_recv_buf[idx].remove(&start).unwrap();
            let skip = (next_expected - start) as usize;
            let useful = &buffered[skip..];
            self.crypto_recv_offset[idx] += useful.len() as u64;
            match &mut self.tls {
                QuicTls::Client(c) => c.read_hs(useful).map_err(QuicError::Tls)?,
                QuicTls::Server(s) => s.read_hs(useful).map_err(QuicError::Tls)?,
            }
        }

        self.apply_peer_transport_parameters()?;

        // Process any TLS output generated by the handshake data we just fed
        self.process_tls_output()?;

        if self.side == Side::Server
            && space == PacketNumberSpace::Handshake
            && self.keys.one_rtt.is_some()
            && self.keys.get(PacketNumberSpace::Handshake).is_some()
            && !self
                .deferred_key_discards
                .contains(&PacketNumberSpace::Handshake)
        {
            self.deferred_key_discards
                .push(PacketNumberSpace::Handshake);
        }

        Ok(())
    }

    // ---- TLS output processing ----

    /// Extract TLS handshake data from rustls and queue CRYPTO frames.
    /// Handles key changes correctly by processing data per encryption level.
    pub(crate) fn process_tls_output(&mut self) -> Result<(), QuicError> {
        self.refresh_zero_rtt_keys();

        let mut buf = Vec::new();
        let mut space = match self.state {
            ConnectionState::Initial => PacketNumberSpace::Initial,
            ConnectionState::Handshake => PacketNumberSpace::Handshake,
            _ => PacketNumberSpace::ApplicationData,
        };

        loop {
            let prev_len = buf.len();

            let key_change = match &mut self.tls {
                QuicTls::Client(c) => c.write_hs(&mut buf),
                QuicTls::Server(s) => s.write_hs(&mut buf),
            };

            // Queue CRYPTO frame for data written at the current space
            let data = buf[prev_len..].to_vec();
            if !data.is_empty() {
                // Cache the Initial CRYPTO data (ClientHello) so we can re-queue
                // it after Retry — rustls drains write_hs() and won't re-emit it.
                if space == PacketNumberSpace::Initial && self.initial_crypto_data.is_none() {
                    self.initial_crypto_data = Some(data.clone());
                }
                let offset = self.crypto_send_offset[space as usize];
                self.crypto_send_offset[space as usize] += data.len() as u64;
                self.pending_frames
                    .push_back((space, Frame::Crypto(CryptoFrame { offset, data })));
            }

            match key_change {
                Some(kc) => {
                    self.install_keys(kc);
                    // Advance to next encryption level
                    space = match space {
                        PacketNumberSpace::Initial => PacketNumberSpace::Handshake,
                        _ => PacketNumberSpace::ApplicationData,
                    };
                }
                None => break,
            }
        }

        self.refresh_zero_rtt_keys();
        self.apply_peer_transport_parameters()?;
        self.finalize_client_early_data_state();

        Ok(())
    }

    fn queue_new_token_frame(&mut self) {
        if self.side != Side::Server || self.new_token_sent || self.keys.one_rtt.is_none() {
            return;
        }

        let Some(secret) = self.new_token_secret.as_ref() else {
            return;
        };

        self.pending_frames.push_back((
            PacketNumberSpace::ApplicationData,
            Frame::NewToken {
                token: retry::encode_new_token(&self.peer_addr, secret),
            },
        ));
        self.new_token_sent = true;
    }

    /// Install new keys from a TLS key change event.
    ///
    /// Key discards are deferred until after `send_pending_frames` has sent
    /// any remaining frames for the old space (QUIC-15 fix).
    fn install_keys(&mut self, key_change: quic::KeyChange) {
        match key_change {
            quic::KeyChange::Handshake { keys } => {
                Self::trace_event("install handshake keys");
                self.keys
                    .set_from_quic_keys(PacketNumberSpace::Handshake, keys);
                // Defer Initial key discard until after pending Initial frames are sent
                self.deferred_key_discards.push(PacketNumberSpace::Initial);
                if self.state == ConnectionState::Initial {
                    self.state = ConnectionState::Handshake;
                }
            }
            quic::KeyChange::OneRtt { keys, next } => {
                Self::trace_event("install 1-rtt keys");
                self.keys
                    .set_from_quic_keys(PacketNumberSpace::ApplicationData, keys);
                // QUIC-10: Store next secrets for key rotation (RFC 9001 §6)
                self.next_secrets = Some(next);
                self.local_key_phase = false;
                self.remote_key_phase = false;
                self.pending_remote_key_update = None;
                self.one_rtt_packets_sent_with_current_keys = 0;
                self.key_update_pn = 0;
                self.state = ConnectionState::Connected;
                if self.side == Side::Server {
                    // Server sends HANDSHAKE_DONE to client (RFC 9001 §4.1.2)
                    self.pending_frames
                        .push_back((PacketNumberSpace::ApplicationData, Frame::HandshakeDone));
                    self.queue_new_token_frame();
                }
                if self.side == Side::Client {
                    // The client can discard Handshake keys once 1-RTT is available.
                    self.deferred_key_discards
                        .push(PacketNumberSpace::Handshake);
                }
            }
        }
    }

    /// Process deferred key discards after all pending frames have been sent.
    fn process_deferred_key_discards(&mut self) {
        for space in self.deferred_key_discards.drain(..) {
            self.keys.discard(space);
            for pkt in self.loss_detection.take_sent_packets(space) {
                self.congestion.on_packet_discarded(pkt.size);
            }
            self.sent_frames[space as usize].clear();
            self.recv_ecn_counts[space as usize] = EcnCounts::default();
            self.largest_recv_pn_time[space as usize] = None;
            self.peer_ack_ecn_counts[space as usize] = None;
            self.sent_ecn_counts[space as usize] = EcnCounts::default();
        }
    }

    // ---- Send path ----

    /// Generate ACK frames for spaces that need acknowledgment.
    fn generate_acks(&mut self) {
        let now = Instant::now();

        for space_idx in 0..3 {
            if !self.ack_needed[space_idx] || self.recv_pn_ranges[space_idx].is_empty() {
                continue;
            }

            let space = match space_idx {
                0 => PacketNumberSpace::Initial,
                1 => PacketNumberSpace::Handshake,
                _ => PacketNumberSpace::ApplicationData,
            };

            // Skip if we don't have keys for this space
            if space == PacketNumberSpace::ApplicationData {
                if self.application_send_kind().is_none() {
                    continue;
                }
            } else if self.keys.get(space).is_none() {
                continue;
            }

            let should_send = match space {
                PacketNumberSpace::Initial | PacketNumberSpace::Handshake => true,
                PacketNumberSpace::ApplicationData => {
                    self.ack_immediate[space_idx]
                        || self.ack_eliciting_packets_since_last_ack[space_idx] >= 2
                        || self.last_ack_eliciting_recv[space_idx]
                            .is_some_and(|t| now.duration_since(t) >= self.local_max_ack_delay)
                }
            };
            if !should_send {
                continue;
            }

            let ranges: Vec<AckRange> = self.recv_pn_ranges[space_idx]
                .iter()
                .map(|&(start, end)| AckRange { start, end })
                .collect();

            if ranges.is_empty() {
                continue;
            }

            // Compute ACK delay, scaled by 2^local_ack_delay_exponent (RFC 9000 §19.3)
            let ack_delay = self.largest_recv_pn_time[space_idx]
                .map(|t| now.duration_since(t).as_micros() as u64 >> self.local_ack_delay_exponent)
                .unwrap_or(0);

            let ack = AckFrame {
                largest_acknowledged: ranges[0].end,
                ack_delay,
                ranges,
                ecn: self.ack_ecn_counts(space),
            };

            self.pending_frames.push_front((space, Frame::Ack(ack)));
            self.ack_needed[space_idx] = false;
            self.ack_eliciting_packets_since_last_ack[space_idx] = 0;
            self.ack_immediate[space_idx] = false;
        }
    }

    /// Generate MAX_DATA and MAX_STREAM_DATA frames when flow control
    /// windows are more than half consumed (RFC 9000 §4.2).
    fn generate_flow_control_updates(&mut self) {
        // Connection-level MAX_DATA
        if let Some(new_max) = self.flow_control.should_send_max_data() {
            self.flow_control.commit_max_data(new_max);
            self.pending_frames.push_back((
                PacketNumberSpace::ApplicationData,
                Frame::MaxData {
                    maximum_data: new_max,
                },
            ));
        }

        // Per-stream MAX_STREAM_DATA
        let stream_ids: Vec<StreamId> = self.streams.stream_ids().copied().collect();
        for sid in stream_ids {
            if let Some(stream) = self.streams.get_mut(sid) {
                if stream.recv.is_none() {
                    continue;
                }
                if let Some(new_max) = stream.flow_control.should_send_max_stream_data() {
                    stream.flow_control.commit_max_stream_data(new_max);
                    self.pending_frames.push_back((
                        PacketNumberSpace::ApplicationData,
                        Frame::MaxStreamData {
                            stream_id: sid,
                            maximum_stream_data: new_max,
                        },
                    ));
                }
            }
        }
    }

    fn generate_stream_limit_updates(&mut self) {
        let (bidi, uni) = self.streams.take_max_streams_updates();
        if let Some(maximum_streams) = bidi {
            self.pending_frames.push_back((
                PacketNumberSpace::ApplicationData,
                Frame::MaxStreams {
                    bidirectional: true,
                    maximum_streams,
                },
            ));
        }
        if let Some(maximum_streams) = uni {
            self.pending_frames.push_back((
                PacketNumberSpace::ApplicationData,
                Frame::MaxStreams {
                    bidirectional: false,
                    maximum_streams,
                },
            ));
        }
    }

    fn generate_connection_id_updates(&mut self) {
        if !self.peer_transport_parameters_applied
            || self.state != ConnectionState::Connected
            || self.cid_manager.cid_len() == 0
        {
            return;
        }

        let target_active_local_cids = self.peer_active_connection_id_limit.min(2) as usize;
        while self.cid_manager.active_local_cid_count() < target_active_local_cids {
            self.cid_manager.issue_new_cid();
        }

        for sequence_number in self.cid_manager.take_peer_cids_to_retire() {
            self.pending_frames.push_back((
                PacketNumberSpace::ApplicationData,
                Frame::RetireConnectionId { sequence_number },
            ));
        }

        let retire_prior_to = self.cid_manager.local_retire_prior_to();
        for entry in self.cid_manager.take_unadvertised_local_cids() {
            if let Some(stateless_reset_token) = entry.stateless_reset_token {
                self.pending_frames.push_back((
                    PacketNumberSpace::ApplicationData,
                    Frame::NewConnectionId {
                        sequence_number: entry.sequence_number,
                        retire_prior_to: retire_prior_to.min(entry.sequence_number),
                        connection_id: entry.cid.as_slice().to_vec(),
                        stateless_reset_token,
                    },
                ));
            }
        }
    }

    /// Split a CRYPTO or STREAM frame so the head fits within `budget` bytes.
    ///
    /// Returns `Some((head, tail))` if the frame can be split, `None` if it is
    /// not a data-carrying frame or the data already fits within the budget.
    fn split_frame(frame: Frame, budget: usize) -> Option<(Frame, Frame)> {
        match frame {
            Frame::Crypto(cf) => {
                if cf.data.len() <= 1 {
                    return None;
                }
                let mut full_buf = Vec::new();
                Frame::Crypto(cf.clone()).encode(&mut full_buf).ok()?;
                if full_buf.len() <= budget {
                    return None;
                }
                let mut low = 1usize;
                let mut high = cf.data.len() - 1;
                let mut split_at = 0usize;
                while low <= high {
                    let mid = low + (high - low) / 2;
                    let candidate = Frame::Crypto(frame::crypto::CryptoFrame {
                        offset: cf.offset,
                        data: cf.data[..mid].to_vec(),
                    });
                    let mut candidate_buf = Vec::new();
                    candidate.encode(&mut candidate_buf).ok()?;
                    if candidate_buf.len() <= budget {
                        split_at = mid;
                        low = mid + 1;
                    } else {
                        high = mid.saturating_sub(1);
                    }
                }
                if split_at == 0 {
                    return None;
                }
                let head = Frame::Crypto(frame::crypto::CryptoFrame {
                    offset: cf.offset,
                    data: cf.data[..split_at].to_vec(),
                });
                let tail = Frame::Crypto(frame::crypto::CryptoFrame {
                    offset: cf.offset + split_at as u64,
                    data: cf.data[split_at..].to_vec(),
                });
                Some((head, tail))
            }
            Frame::Stream(sf) => {
                if sf.data.len() <= 1 {
                    return None;
                }
                let mut full_buf = Vec::new();
                Frame::Stream(sf.clone()).encode(&mut full_buf).ok()?;
                if full_buf.len() <= budget {
                    return None;
                }
                let mut low = 1usize;
                let mut high = sf.data.len() - 1;
                let mut split_at = 0usize;
                while low <= high {
                    let mid = low + (high - low) / 2;
                    let candidate = Frame::Stream(frame::stream::StreamFrame {
                        stream_id: sf.stream_id,
                        offset: sf.offset,
                        fin: false,
                        data: sf.data[..mid].to_vec(),
                    });
                    let mut candidate_buf = Vec::new();
                    candidate.encode(&mut candidate_buf).ok()?;
                    if candidate_buf.len() <= budget {
                        split_at = mid;
                        low = mid + 1;
                    } else {
                        high = mid.saturating_sub(1);
                    }
                }
                if split_at == 0 {
                    return None;
                }
                let head = Frame::Stream(frame::stream::StreamFrame {
                    stream_id: sf.stream_id,
                    offset: sf.offset,
                    fin: false, // FIN only on the last fragment
                    data: sf.data[..split_at].to_vec(),
                });
                let tail = Frame::Stream(frame::stream::StreamFrame {
                    stream_id: sf.stream_id,
                    offset: sf.offset + split_at as u64,
                    fin: sf.fin,
                    data: sf.data[split_at..].to_vec(),
                });
                Some((head, tail))
            }
            _ => None,
        }
    }

    fn build_packet_to_addr(
        &mut self,
        space: PacketNumberSpace,
        packet_frames: Vec<Frame>,
        mut payload: Vec<u8>,
        peer_addr: SocketAddr,
        track_retransmission: bool,
        pending_datagram_bytes: usize,
    ) -> Result<Option<BuiltPacket>, QuicError> {
        let app_kind = if space == PacketNumberSpace::ApplicationData {
            Some(match self.application_send_kind() {
                Some(kind) => kind,
                None => return Ok(None),
            })
        } else {
            None
        };
        let tag_len = match app_kind {
            Some(ApplicationPacketKind::ZeroRtt) => self
                .keys
                .zero_rtt_local()
                .map(|k| k.packet.tag_len())
                .unwrap_or(16),
            _ => self
                .keys
                .get(space)
                .map(|k| k.local.tag_len())
                .unwrap_or(16),
        };
        let sample_len = match app_kind {
            Some(ApplicationPacketKind::ZeroRtt) => self
                .keys
                .zero_rtt_local()
                .map(|k| k.header.sample_len())
                .unwrap_or(16),
            _ => self
                .keys
                .get(space)
                .map(|k| k.local_header.sample_len())
                .unwrap_or(16),
        };
        let is_ack_eliciting = packet_frames.iter().any(|f| f.is_ack_eliciting());
        let is_close_only = !packet_frames.is_empty()
            && packet_frames
                .iter()
                .all(|frame| matches!(frame, Frame::ConnectionClose { .. }));
        let congestion_controlled = !is_close_only;
        let estimated_size = payload.len() + PACKET_OVERHEAD + tag_len;
        if congestion_controlled
            && is_ack_eliciting
            && !self
                .congestion
                .can_send(estimated_size + pending_datagram_bytes)
        {
            return Ok(None);
        }

        let pn = self.next_pn[space as usize];
        let largest_acked = self.loss_detection.largest_acked(space).unwrap_or(0);

        let dcid = self.peer_cid_for_addr(peer_addr);
        let local_cid = self.cid_manager.primary_local_cid().clone();
        let encode_packet = |payload: &[u8]| -> Result<(Vec<u8>, usize, u8), QuicError> {
            match space {
                PacketNumberSpace::Initial => {
                    let token = &self.retry_token;
                    let (trial, _, _) = crate::packet::initial::encode_initial_packet(
                        &dcid,
                        &local_cid,
                        token,
                        pn,
                        largest_acked,
                        payload,
                        tag_len,
                    )?;
                    let wire_size = trial.len() + tag_len;
                    let mut payload = payload.to_vec();
                    if wire_size < crate::packet::initial::INITIAL_MIN_DATAGRAM_SIZE {
                        let pad = crate::packet::initial::INITIAL_MIN_DATAGRAM_SIZE - wire_size;
                        payload.resize(payload.len() + pad, 0x00);
                    }
                    crate::packet::initial::encode_initial_packet(
                        &dcid,
                        &local_cid,
                        token,
                        pn,
                        largest_acked,
                        &payload,
                        tag_len,
                    )
                }
                PacketNumberSpace::Handshake => crate::packet::handshake::encode_handshake_packet(
                    &dcid,
                    &local_cid,
                    pn,
                    largest_acked,
                    payload,
                    tag_len,
                ),
                PacketNumberSpace::ApplicationData => match app_kind {
                    Some(ApplicationPacketKind::ZeroRtt) => {
                        crate::packet::zero_rtt::encode_zero_rtt_packet(
                            &dcid,
                            &local_cid,
                            pn,
                            largest_acked,
                            payload,
                            tag_len,
                        )
                    }
                    Some(ApplicationPacketKind::OneRtt) => {
                        Ok(crate::packet::one_rtt::encode_one_rtt_packet(
                            &dcid,
                            false,
                            self.local_key_phase,
                            pn,
                            largest_acked,
                            payload,
                        ))
                    }
                    None => unreachable!(),
                },
            }
        };

        let (mut packet_data, mut pn_offset, mut pn_len) = encode_packet(&payload)?;
        let min_plaintext_len = sample_len
            .saturating_add(4)
            .saturating_sub(pn_len as usize)
            .saturating_sub(tag_len);
        if payload.len() < min_plaintext_len {
            payload.resize(min_plaintext_len, 0x00);
            (packet_data, pn_offset, pn_len) = encode_packet(&payload)?;
        }

        let packet_len = packet_data.len().saturating_add(tag_len);
        // INVARIANT (AEAD nonce uniqueness): the amplification/congestion gates below MUST
        // run before encryption and before `next_pn` is advanced. The packet number is the
        // AEAD nonce; encrypting a packet that is then dropped without advancing `next_pn`
        // would reuse the same packet number — and thus the same nonce — on the next send,
        // breaking AEAD. Never reorder `encrypt_packet` or the `next_pn += 1` above these gates.
        if !self.can_send_to_addr(peer_addr, pending_datagram_bytes.saturating_add(packet_len)) {
            return Ok(None);
        }

        let header_end = pn_offset + pn_len as usize;
        let header = packet_data[..header_end].to_vec();
        let mut payload_bytes = packet_data[header_end..].to_vec();

        let mut send_data = header;
        {
            let (local_packet_key, local_header_key) = match app_kind {
                Some(ApplicationPacketKind::ZeroRtt) => match self.keys.zero_rtt_local() {
                    Some(k) => (k.packet.as_ref(), k.header.as_ref()),
                    None => return Ok(None),
                },
                _ => match self.keys.get(space) {
                    Some(k) => (k.local.as_ref(), k.local_header.as_ref()),
                    None => return Ok(None),
                },
            };

            aead::encrypt_packet(pn, &send_data, &mut payload_bytes, local_packet_key)?;
            send_data.extend_from_slice(&payload_bytes);

            header_protection::apply_header_protection(
                &mut send_data,
                pn_offset,
                pn_len as usize,
                local_header_key,
            )?;
        }

        if app_kind == Some(ApplicationPacketKind::ZeroRtt) {
            self.attempted_zero_rtt = true;
        }
        // Advance only after the packet cleared the gates and was actually built (nonce uniqueness — see above).
        self.next_pn[space as usize] += 1;
        Ok(Some(BuiltPacket {
            space,
            pn,
            frames: packet_frames,
            bytes: send_data,
            ack_eliciting: is_ack_eliciting,
            track_retransmission,
            congestion_controlled,
            app_kind,
        }))
    }

    async fn send_built_datagram_to_addr(
        &mut self,
        peer_addr: SocketAddr,
        mut datagram: Vec<u8>,
        packets: Vec<BuiltPacket>,
    ) -> Result<(), QuicError> {
        if datagram.is_empty() {
            return Ok(());
        }

        if let Some(deadline) = self.next_paced_send_time {
            let now = Instant::now();
            if deadline > now {
                cooperative_sleep(deadline.saturating_duration_since(now)).await;
            }
        }

        let ecn = (self.outbound_ecn_enabled
            && packets
                .iter()
                .all(|packet| packet.space == PacketNumberSpace::ApplicationData))
        .then_some(UdpEcnCodepoint::Ect0);
        self.socket
            .send_to_with_ecn(peer_addr, &mut datagram, ecn)
            .await?;

        let now = Instant::now();
        let ack_eliciting_datagram = packets.iter().any(|packet| packet.ack_eliciting);
        self.record_packet_sent(now, ack_eliciting_datagram);
        self.record_unvalidated_bytes_sent(peer_addr, datagram.len());

        for packet in packets {
            if packet.ack_eliciting {
                let sent_ecn_counts = &mut self.sent_ecn_counts[packet.space as usize];
                match ecn {
                    Some(UdpEcnCodepoint::Ect0) => {
                        sent_ecn_counts.ect0 = sent_ecn_counts.ect0.saturating_add(1);
                    }
                    Some(UdpEcnCodepoint::Ect1) => {
                        sent_ecn_counts.ect1 = sent_ecn_counts.ect1.saturating_add(1);
                    }
                    _ => {}
                }
            }
            let frame_kinds: Vec<&'static str> =
                packet.frames.iter().map(Self::frame_kind).collect();
            Self::trace_event(format!(
                "send {:?} pn={} len={} ack_eliciting={} frames={:?}",
                packet.space,
                packet.pn,
                packet.bytes.len(),
                packet.ack_eliciting,
                frame_kinds
            ));
            if packet.track_retransmission {
                self.sent_frames[packet.space as usize].insert(packet.pn, packet.frames);
                self.loss_detection.on_packet_sent(SentPacket {
                    pn: packet.pn,
                    space: packet.space,
                    time_sent: now,
                    size: packet.bytes.len(),
                    ack_eliciting: packet.ack_eliciting,
                    ecn_marking: ecn,
                });
                if packet.app_kind == Some(ApplicationPacketKind::ZeroRtt) {
                    self.zero_rtt_in_flight.insert(packet.pn);
                }
            }
            if packet.app_kind == Some(ApplicationPacketKind::OneRtt) {
                self.one_rtt_packets_sent_with_current_keys = self
                    .one_rtt_packets_sent_with_current_keys
                    .saturating_add(1);
            }
            if packet.congestion_controlled {
                self.congestion.on_packet_sent(packet.bytes.len(), now);
            }
        }

        let pacing_window = self.congestion.window().max(datagram.len()) as f64;
        let pacing_delay = self
            .loss_detection
            .rtt
            .smoothed_rtt
            .mul_f64(datagram.len() as f64 / pacing_window);
        self.next_paced_send_time = Some(now + pacing_delay.max(Duration::from_micros(1)));

        Ok(())
    }

    async fn send_frames_to_addr(
        &mut self,
        space: PacketNumberSpace,
        packet_frames: Vec<Frame>,
        payload: Vec<u8>,
        peer_addr: SocketAddr,
        track_retransmission: bool,
    ) -> Result<bool, QuicError> {
        let Some(packet) = self.build_packet_to_addr(
            space,
            packet_frames,
            payload,
            peer_addr,
            track_retransmission,
            0,
        )?
        else {
            return Ok(false);
        };

        let datagram = packet.bytes.clone();
        self.send_built_datagram_to_addr(peer_addr, datagram, vec![packet])
            .await?;

        Ok(true)
    }

    async fn flush_pending_connection_close(&mut self) -> Result<(), QuicError> {
        if self.state == ConnectionState::Closing && self.closing_response_pending {
            let close_frames = self.closing_frames.clone();
            self.closing_response_pending =
                !self.send_connection_close_frames(&close_frames).await?;
        }
        Ok(())
    }

    async fn send_connection_close_frames(
        &mut self,
        close_frames: &[(PacketNumberSpace, Frame)],
    ) -> Result<bool, QuicError> {
        let mut sent_all = true;

        for (space, frame) in close_frames {
            let mut payload = Vec::new();
            frame.encode(&mut payload)?;
            if !self
                .send_frames_to_addr(*space, vec![frame.clone()], payload, self.peer_addr, false)
                .await?
            {
                sent_all = false;
            }
        }

        Ok(sent_all)
    }

    async fn send_targeted_pending_frames(&mut self) -> Result<(), QuicError> {
        let mut remaining = VecDeque::new();

        while let Some((peer_addr, space, frame)) = self.pending_targeted_frames.pop_front() {
            if space == PacketNumberSpace::ApplicationData {
                match self.application_send_kind() {
                    None => {
                        remaining.push_back((peer_addr, space, frame));
                        continue;
                    }
                    Some(ApplicationPacketKind::ZeroRtt)
                        if !Self::frame_allowed_in_zero_rtt(&frame) =>
                    {
                        remaining.push_back((peer_addr, space, frame));
                        continue;
                    }
                    Some(ApplicationPacketKind::ZeroRtt | ApplicationPacketKind::OneRtt) => {}
                }
            } else if self.keys.get(space).is_none() {
                remaining.push_back((peer_addr, space, frame));
                continue;
            }
            if space == PacketNumberSpace::ApplicationData {
                self.maybe_initiate_key_update()?;
            }

            let mut payload = Vec::new();
            frame.encode(&mut payload)?;
            if !self
                .send_frames_to_addr(space, vec![frame.clone()], payload, peer_addr, false)
                .await?
            {
                remaining.push_back((peer_addr, space, frame));
                while let Some(item) = self.pending_targeted_frames.pop_front() {
                    remaining.push_back(item);
                }
                break;
            }
        }

        self.pending_targeted_frames = remaining;

        Ok(())
    }

    fn build_path_mtu_probe_packet(
        &mut self,
        target_size: usize,
    ) -> Result<Option<BuiltPacket>, QuicError> {
        let space = PacketNumberSpace::ApplicationData;
        let Some(keys) = self.keys.get(space) else {
            return Ok(None);
        };

        let pn = self.next_pn[space as usize];
        let largest_acked = self.loss_detection.largest_acked(space).unwrap_or(0);
        let dcid = self.cid_manager.active_peer_cid().clone();
        let (base_packet, _, _) = crate::packet::one_rtt::encode_one_rtt_packet(
            &dcid,
            false,
            self.local_key_phase,
            pn,
            largest_acked,
            &[0x01],
        );
        let base_size = base_packet.len() + keys.local.tag_len();
        if target_size <= base_size {
            return Ok(None);
        }

        let mut payload = vec![0x01];
        payload.resize(payload.len() + (target_size - base_size), 0x00);
        self.build_packet_to_addr(space, vec![Frame::Ping], payload, self.peer_addr, true, 0)
    }

    async fn send_path_mtu_probe(&mut self) -> Result<(), QuicError> {
        let now = Instant::now();
        if !self.path_mtu_probe_due(now) {
            return Ok(());
        }

        let Some(target_size) = self.next_path_mtu_probe_size() else {
            return Ok(());
        };

        let Some(packet) = self.build_path_mtu_probe_packet(target_size)? else {
            self.schedule_next_path_mtu_probe(now);
            return Ok(());
        };

        let pn = packet.pn;
        let probe_size = packet.bytes.len();
        let datagram = packet.bytes.clone();
        self.send_built_datagram_to_addr(self.peer_addr, datagram, vec![packet])
            .await?;
        self.path_mtu.in_flight_probe = Some(PathMtuProbe {
            pn,
            size: probe_size,
            sent_at: Instant::now(),
        });
        self.path_mtu.next_probe_at = None;

        Ok(())
    }

    /// Send all pending frames as QUIC packets with AEAD protection.
    ///
    /// Coalesces multiple frames into a single packet up to the MTU,
    /// fragments CRYPTO/STREAM frames that exceed the payload budget,
    /// checks the congestion window before sending, and records frames
    /// for retransmission on loss.
    pub(crate) async fn send_pending_frames(&mut self) -> Result<(), QuicError> {
        // Reap terminal streams before advertising replenished stream credit.
        self.streams.gc_closed_streams();

        // Generate ACK frames first
        self.generate_acks();

        // Generate flow control updates
        self.generate_flow_control_updates();

        // Generate MAX_STREAMS replenishment.
        self.generate_stream_limit_updates();

        // Generate NEW_CONNECTION_ID / RETIRE_CONNECTION_ID frames.
        self.generate_connection_id_updates();

        let mut coalesced_datagram = Vec::new();
        let mut coalesced_packets = Vec::new();

        // Send one or more packets per encryption space.
        for space_idx in 0..3usize {
            let space = match space_idx {
                0 => PacketNumberSpace::Initial,
                1 => PacketNumberSpace::Handshake,
                _ => PacketNumberSpace::ApplicationData,
            };

            if space == PacketNumberSpace::ApplicationData {
                if self.application_send_kind().is_none() {
                    continue;
                }
            } else if self.keys.get(space).is_none() {
                continue;
            }

            // Separate frames for this space from other spaces.
            let mut space_frames: VecDeque<Frame> = VecDeque::new();
            let mut other_frames = VecDeque::new();
            while let Some((s, frame)) = self.pending_frames.pop_front() {
                if s == space {
                    space_frames.push_back(frame);
                } else {
                    other_frames.push_back((s, frame));
                }
            }
            self.pending_frames = other_frames;

            if space == PacketNumberSpace::ApplicationData
                && matches!(
                    self.application_send_kind(),
                    Some(ApplicationPacketKind::ZeroRtt)
                )
            {
                let mut zero_rtt_frames = VecDeque::new();
                while let Some(frame) = space_frames.pop_front() {
                    if Self::frame_allowed_in_zero_rtt(&frame) {
                        zero_rtt_frames.push_back(frame);
                    } else {
                        self.pending_frames.push_back((space, frame));
                    }
                }
                space_frames = zero_rtt_frames;
            }

            // Send packets until all frames for this space are consumed
            // (or the congestion window blocks us).
            while !space_frames.is_empty() {
                if space == PacketNumberSpace::ApplicationData {
                    self.maybe_initiate_key_update()?;
                }
                let tag_len = if space == PacketNumberSpace::ApplicationData {
                    match self.application_send_kind() {
                        Some(ApplicationPacketKind::ZeroRtt) => self
                            .keys
                            .zero_rtt_local()
                            .map(|k| k.packet.tag_len())
                            .unwrap_or(16),
                        _ => self
                            .keys
                            .get(space)
                            .map(|k| k.local.tag_len())
                            .unwrap_or(16),
                    }
                } else {
                    self.keys
                        .get(space)
                        .map(|k| k.local.tag_len())
                        .unwrap_or(16)
                };
                let max_payload = self
                    .max_send_udp_payload_size
                    .saturating_sub(PACKET_OVERHEAD)
                    .saturating_sub(tag_len);

                let mut packet_frames: Vec<Frame> = Vec::new();
                let mut payload = Vec::new();

                while let Some(frame) = space_frames.pop_front() {
                    let mut trial_buf = Vec::new();
                    frame.encode(&mut trial_buf)?;
                    let budget = max_payload.saturating_sub(payload.len());

                    if trial_buf.len() <= budget {
                        // Frame fits entirely.
                        payload.extend_from_slice(&trial_buf);
                        packet_frames.push(frame);
                    } else if budget > 0 && matches!(&frame, Frame::Crypto(_) | Frame::Stream(_)) {
                        // Data-carrying frame too large: split at budget boundary.
                        if let Some((head, tail)) = Self::split_frame(frame, budget) {
                            let mut head_buf = Vec::new();
                            head.encode(&mut head_buf)?;
                            payload.extend_from_slice(&head_buf);
                            packet_frames.push(head);
                            space_frames.push_front(tail);
                        }
                        break; // packet full
                    } else if payload.is_empty() {
                        // Non-splittable frame as first frame — accept it to avoid
                        // an infinite loop (control frames are always small enough).
                        payload.extend_from_slice(&trial_buf);
                        packet_frames.push(frame);
                    } else {
                        // Doesn't fit alongside existing payload — defer to next packet.
                        space_frames.push_front(frame);
                        break; // flush current packet first
                    }
                }

                if packet_frames.is_empty() {
                    break;
                }

                let pending_bytes = if space == PacketNumberSpace::ApplicationData {
                    0
                } else {
                    coalesced_datagram.len()
                };

                let Some(packet) = self.build_packet_to_addr(
                    space,
                    packet_frames.clone(),
                    payload,
                    self.peer_addr,
                    true,
                    pending_bytes,
                )?
                else {
                    // Re-queue frames — we're congestion-limited
                    for f in packet_frames.into_iter().rev() {
                        space_frames.push_front(f);
                    }
                    break; // stop sending for this space
                };

                if space == PacketNumberSpace::ApplicationData {
                    if !coalesced_packets.is_empty() {
                        let datagram = std::mem::take(&mut coalesced_datagram);
                        let packets = std::mem::take(&mut coalesced_packets);
                        self.send_built_datagram_to_addr(self.peer_addr, datagram, packets)
                            .await?;
                    }
                    let datagram = packet.bytes.clone();
                    self.send_built_datagram_to_addr(self.peer_addr, datagram, vec![packet])
                        .await?;
                } else if coalesced_datagram.len() + packet.bytes.len()
                    <= self.max_send_udp_payload_size
                {
                    coalesced_datagram.extend_from_slice(&packet.bytes);
                    coalesced_packets.push(packet);
                } else {
                    if !coalesced_packets.is_empty() {
                        let datagram = std::mem::take(&mut coalesced_datagram);
                        let packets = std::mem::take(&mut coalesced_packets);
                        self.send_built_datagram_to_addr(self.peer_addr, datagram, packets)
                            .await?;
                    }
                    coalesced_datagram = packet.bytes.clone();
                    coalesced_packets.push(packet);
                }
            }

            // Re-queue any unsent frames (congestion-limited).
            for f in space_frames {
                self.pending_frames.push_back((space, f));
            }
        }

        if !coalesced_packets.is_empty() {
            let datagram = std::mem::take(&mut coalesced_datagram);
            let packets = std::mem::take(&mut coalesced_packets);
            self.send_built_datagram_to_addr(self.peer_addr, datagram, packets)
                .await?;
        }

        self.send_targeted_pending_frames().await?;
        self.send_path_mtu_probe().await?;

        // Discard keys for spaces that have been superseded (QUIC-15)
        self.process_deferred_key_discards();

        Ok(())
    }

    // ---- PN range tracking for ACK generation ----

    /// Record a received packet number for ACK generation.
    /// Ranges are stored in descending order of `end` values.
    fn record_recv_pn(&mut self, space: PacketNumberSpace, pn: u64) {
        let ranges = &mut self.recv_pn_ranges[space as usize];
        let insert_idx = ranges
            .iter()
            .position(|&(_, end)| end < pn)
            .unwrap_or(ranges.len());

        if insert_idx > 0 {
            let (start, end) = ranges[insert_idx - 1];
            if pn >= start && pn <= end {
                return;
            }
        }
        if insert_idx < ranges.len() {
            let (start, end) = ranges[insert_idx];
            if pn >= start && pn <= end {
                return;
            }
        }

        let mut merged_start = pn;
        let mut merged_end = pn;
        let mut remove_from = insert_idx;
        let mut remove_to = insert_idx;

        if insert_idx > 0 {
            let (start, end) = ranges[insert_idx - 1];
            if Self::packet_numbers_are_adjacent(pn, start) {
                merged_end = end;
                remove_from = insert_idx - 1;
            }
        }

        if insert_idx < ranges.len() {
            let (start, end) = ranges[insert_idx];
            if Self::packet_numbers_are_adjacent(end, merged_start) {
                merged_start = start;
                merged_end = merged_end.max(end);
                remove_to = insert_idx + 1;
            }
        }

        ranges.splice(remove_from..remove_to, [(merged_start, merged_end)]);
        if ranges.len() > MAX_TRACKED_RECV_PN_RANGES {
            ranges.truncate(MAX_TRACKED_RECV_PN_RANGES);
        }
    }

    fn packet_numbers_are_adjacent(lower: u64, higher: u64) -> bool {
        lower.checked_add(1) == Some(higher)
    }

    fn buffer_stream_recv_data(recv_buf: &mut HashMap<u64, Vec<u8>>, offset: u64, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }

        let mut pending_segments = vec![(offset, data)];
        let mut existing_ranges: Vec<(u64, u64)> = recv_buf
            .iter()
            .map(|(&start, data)| (start, start + data.len() as u64))
            .collect();
        existing_ranges.sort_by_key(|(start, _)| *start);

        for (existing_start, existing_end) in existing_ranges {
            let mut next_segments = Vec::new();
            for (segment_start, segment_data) in pending_segments {
                let segment_end = segment_start + segment_data.len() as u64;
                let overlap_start = segment_start.max(existing_start);
                let overlap_end = segment_end.min(existing_end);

                if overlap_start >= overlap_end {
                    next_segments.push((segment_start, segment_data));
                    continue;
                }

                if segment_start < overlap_start {
                    let prefix_len = (overlap_start - segment_start) as usize;
                    next_segments.push((segment_start, segment_data[..prefix_len].to_vec()));
                }
                if overlap_end < segment_end {
                    let suffix_offset = (overlap_end - segment_start) as usize;
                    next_segments.push((overlap_end, segment_data[suffix_offset..].to_vec()));
                }
            }

            pending_segments = next_segments;
            if pending_segments.is_empty() {
                return;
            }
        }

        for (segment_start, segment_data) in pending_segments {
            recv_buf.insert(segment_start, segment_data);
        }
        Self::coalesce_stream_recv_data(recv_buf);
    }

    fn coalesce_stream_recv_data(recv_buf: &mut HashMap<u64, Vec<u8>>) {
        let mut segments: Vec<(u64, Vec<u8>)> = recv_buf.drain().collect();
        segments.sort_by_key(|(start, _)| *start);

        let mut merged: Vec<(u64, Vec<u8>)> = Vec::with_capacity(segments.len());
        for (start, data) in segments {
            if let Some((last_start, last_data)) = merged.last_mut() {
                let last_end = *last_start + last_data.len() as u64;
                if start <= last_end {
                    let overlap = (last_end - start) as usize;
                    if overlap < data.len() {
                        last_data.extend_from_slice(&data[overlap..]);
                    }
                    continue;
                }
            }
            merged.push((start, data));
        }

        recv_buf.extend(merged);
    }

    fn stream_has_received_all_data(stream: &StreamState) -> bool {
        let Some(final_size) = stream.final_size else {
            return false;
        };

        if stream.recv_offset >= final_size {
            return true;
        }

        let mut covered_until = stream.recv_offset;
        let mut segments: Vec<(u64, u64)> = stream
            .recv_buf
            .iter()
            .map(|(&start, data)| (start, start + data.len() as u64))
            .collect();
        segments.sort_unstable_by_key(|(start, _)| *start);

        for (start, end) in segments {
            if end <= covered_until {
                continue;
            }
            if start > covered_until {
                return false;
            }
            covered_until = end;
            if covered_until >= final_size {
                return true;
            }
        }

        false
    }

    // ---- QUIC-10: Key update rotation (RFC 9001 §6) ----

    /// Try to decrypt a 1-RTT packet using the next key generation.
    /// Called when the received key phase differs from our current key phase.
    ///
    /// On success, commits the key update: replaces packet keys in the existing
    /// DirectionalKeys (header protection keys never change — RFC 9001 §5.4),
    /// toggles `key_phase`, and advances `next_secrets`.
    fn try_decrypt_with_next_keys(
        &mut self,
        pn: u64,
        recv_key_phase: bool,
        header: &[u8],
        ciphertext: &mut [u8],
    ) -> Result<usize, QuicError> {
        // Don't process key updates for packets older than when we last updated
        if pn < self.key_update_pn {
            return Err(QuicError::InvalidPacket("stale key phase packet".into()));
        }

        if self
            .pending_remote_key_update
            .as_ref()
            .is_some_and(|update| update.key_phase == recv_key_phase)
        {
            let plaintext_len = aead::decrypt_packet(
                pn,
                header,
                ciphertext,
                self.pending_remote_key_update
                    .as_ref()
                    .expect("checked above")
                    .remote
                    .as_ref(),
            )?;

            let pending = self
                .pending_remote_key_update
                .take()
                .expect("checked above");
            if let Some(ref mut dk) = self.keys.one_rtt {
                dk.remote = pending.remote;
            }
            self.remote_key_phase = pending.key_phase;
            self.key_update_pn = pn;

            return Ok(plaintext_len);
        }

        // Derive next keys from secrets (this also advances the secret state).
        let next_keys = self
            .next_secrets
            .as_mut()
            .ok_or_else(|| {
                QuicError::Transport(
                    TransportErrorCode::KeyUpdateError,
                    "no next secrets available".into(),
                )
            })?
            .next_packet_keys();

        // Try to decrypt with the new remote key.
        let plaintext_len =
            aead::decrypt_packet(pn, header, ciphertext, next_keys.remote.as_ref())?;

        // Decryption succeeded — commit the key update.
        // Replace only the packet keys; header protection keys are unchanged (RFC 9001 §5.4).
        if let Some(ref mut dk) = self.keys.one_rtt {
            dk.local = next_keys.local;
            dk.remote = next_keys.remote;
        }

        self.local_key_phase = recv_key_phase;
        self.remote_key_phase = recv_key_phase;
        self.pending_remote_key_update = None;
        self.one_rtt_packets_sent_with_current_keys = 0;
        self.key_update_pn = pn;

        Ok(plaintext_len)
    }
}

// Manual Debug implementation since QuicTls contains dyn types
impl std::fmt::Debug for QuicConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicConnection")
            .field("state", &self.state)
            .field("side", &self.side)
            .field("peer_addr", &self.peer_addr)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{QuicClient, QuicClientConfig};
    use crate::packet::header::{PacketHeader, PacketNumberSpace};
    use crate::packet::retry;
    use crate::recovery::congestion::INITIAL_WINDOW;
    use crate::server::QuicListener;
    use crate::server::QuicServerConfig;
    use crate::transport::flow_control::StreamFlowControl;
    use crate::transport::stream_manager::StreamState;
    use rcgen::{CertifiedKey, KeyPair};
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::{ClientConfig, RootCertStore, ServerConfig};
    use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
    use starfish_reactor::cooperative_synchronization::delayed_future::cooperative_sleep;
    use starfish_reactor::reactor::Reactor;
    use std::collections::HashMap;
    use std::net::Ipv6Addr;
    use std::sync::{Arc, Mutex};

    /// Create a client QuicConnection for testing (no reactor needed).
    fn make_test_client() -> QuicConnection {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let std_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let socket = starfish_reactor::cooperative_io::udp_socket::UdpSocket::new(std_socket);
        let peer_addr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let local_cid = ConnectionId::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let remote_cid =
            ConnectionId::from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x11, 0x22]);

        // Minimal TLS config that doesn't verify server certs (test only)
        let mut tls_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        let mut transport_params = packet::LocalTransportParameters::new(
            DEFAULT_INITIAL_MAX_DATA,
            DEFAULT_INITIAL_MAX_STREAM_DATA,
            DEFAULT_INITIAL_MAX_STREAMS_BIDI,
            DEFAULT_INITIAL_MAX_STREAMS_UNI,
        );
        transport_params.max_idle_timeout_ms = DEFAULT_IDLE_TIMEOUT_MS;
        transport_params.max_udp_payload_size = MAX_LOCAL_UDP_PAYLOAD_SIZE as u64;
        transport_params.initial_source_connection_id = Some(local_cid.clone());

        let transport_params = packet::encode_transport_parameters(&transport_params).unwrap();
        let server_name = rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
        let tls = rustls::quic::ClientConnection::new(
            Arc::new(tls_config),
            rustls::quic::Version::V1,
            server_name,
            transport_params,
        )
        .unwrap();

        let mut conn =
            QuicConnection::new_client(socket, peer_addr, local_cid, remote_cid, tls).unwrap();
        conn.process_tls_output().unwrap();
        conn
    }

    fn enable_application_data(conn: &mut QuicConnection) {
        let provider = rustls::crypto::ring::default_provider();
        let keys =
            crate::crypto::keys::derive_initial_keys(&[1, 2, 3, 4, 5, 6, 7, 8], true, &provider)
                .unwrap();
        conn.keys
            .set_from_quic_keys(PacketNumberSpace::ApplicationData, keys);
        conn.state = ConnectionState::Connected;
        conn.peer_transport_parameters_applied = true;
        conn.path_mtu.peer_max_udp_payload_size = MAX_LOCAL_UDP_PAYLOAD_SIZE;
        conn.path_mtu.probe_upper_bound = MAX_LOCAL_UDP_PAYLOAD_SIZE;
        conn.path_mtu.next_probe_at = Some(Instant::now());
        conn.pending_frames.clear();
    }

    fn enable_handshake_data(conn: &mut QuicConnection) {
        let provider = rustls::crypto::ring::default_provider();
        let keys =
            crate::crypto::keys::derive_initial_keys(&[8, 7, 6, 5, 4, 3, 2, 1], true, &provider)
                .unwrap();
        conn.keys
            .set_from_quic_keys(PacketNumberSpace::Handshake, keys);
        conn.state = ConnectionState::Handshake;
    }

    fn enable_zero_rtt(conn: &mut QuicConnection) {
        let provider = rustls::crypto::ring::default_provider();
        let keys =
            crate::crypto::keys::derive_initial_keys(&[9, 8, 7, 6, 5, 4, 3, 2], true, &provider)
                .unwrap();
        conn.keys.set_zero_rtt(
            true,
            rustls::quic::DirectionalKeys {
                header: keys.local.header,
                packet: keys.local.packet,
            },
        );
        conn.remembered_peer_transport_parameters =
            Some(packet::PeerTransportParameters::default());
    }

    fn make_connected_test_client() -> QuicConnection {
        let mut conn = make_test_client();
        enable_application_data(&mut conn);
        conn
    }

    fn generate_self_signed_cert() -> CertifiedKey {
        let subject_alt_names = vec!["localhost".to_string()];
        let mut params = rcgen::CertificateParams::new(subject_alt_names).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "localhost");

        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        CertifiedKey { cert, key_pair }
    }

    fn make_quic_configs() -> (QuicServerConfig, QuicClientConfig) {
        let certified = generate_self_signed_cert();
        let key_der =
            PrivateKeyDer::from(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));

        let server_tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key_der)
            .unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(certified.cert.der().clone()).unwrap();
        let client_tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        (
            QuicServerConfig::new(server_tls),
            QuicClientConfig::new(client_tls),
        )
    }

    fn arm_path_mtu_probe(conn: &mut QuicConnection) -> BuiltPacket {
        let target_size = conn.next_path_mtu_probe_size().unwrap();
        let packet = conn
            .build_path_mtu_probe_packet(target_size)
            .unwrap()
            .unwrap();
        let sent_at = Instant::now() - Duration::from_millis(1);
        conn.sent_ecn_counts[PacketNumberSpace::ApplicationData as usize].ect0 = conn
            .sent_ecn_counts[PacketNumberSpace::ApplicationData as usize]
            .ect0
            .saturating_add(1);
        conn.sent_frames[PacketNumberSpace::ApplicationData as usize]
            .insert(packet.pn, packet.frames.clone());
        conn.loss_detection
            .on_packet_sent(crate::recovery::loss_detection::SentPacket {
                pn: packet.pn,
                space: packet.space,
                time_sent: sent_at,
                size: packet.bytes.len(),
                ack_eliciting: packet.ack_eliciting,
                ecn_marking: Some(UdpEcnCodepoint::Ect0),
            });
        conn.congestion.on_packet_sent(packet.bytes.len(), sent_at);
        conn.path_mtu.in_flight_probe = Some(PathMtuProbe {
            pn: packet.pn,
            size: packet.bytes.len(),
            sent_at,
        });
        conn.path_mtu.next_probe_at = None;
        packet
    }

    /// No-op TLS certificate verifier for tests.
    #[derive(Debug)]
    struct NoVerify;

    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// Build a valid Retry packet (with correct integrity tag) targeting the given connection.
    fn build_retry_packet(
        original_dcid: &ConnectionId,
        client_dcid: &ConnectionId,
        retry_scid: &ConnectionId,
        token: &[u8],
    ) -> Vec<u8> {
        // Encode the Retry packet without tag (DCID = client's local CID, SCID = new server CID)
        let mut pkt = retry::encode_retry_packet(client_dcid, retry_scid, token);
        let tag = retry::compute_retry_integrity_tag(original_dcid, &pkt).unwrap();
        pkt.extend_from_slice(&tag);
        pkt
    }

    fn build_preferred_address_bytes(
        addr: SocketAddr,
        connection_id: &[u8],
        stateless_reset_token: [u8; 16],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        match addr {
            SocketAddr::V4(v4) => {
                out.extend_from_slice(&v4.ip().octets());
                out.extend_from_slice(&v4.port().to_be_bytes());
                out.extend_from_slice(&Ipv6Addr::UNSPECIFIED.octets());
                out.extend_from_slice(&0u16.to_be_bytes());
            }
            SocketAddr::V6(v6) => {
                out.extend_from_slice(&[0u8; 4]);
                out.extend_from_slice(&0u16.to_be_bytes());
                out.extend_from_slice(&v6.ip().octets());
                out.extend_from_slice(&v6.port().to_be_bytes());
            }
        }
        out.push(connection_id.len() as u8);
        out.extend_from_slice(connection_id);
        out.extend_from_slice(&stateless_reset_token);
        out
    }

    fn make_stream_state() -> StreamState {
        StreamState {
            id: 1,
            send: Some(SendState::Ready),
            recv: Some(RecvState::Recv),
            flow_control: StreamFlowControl::new(
                DEFAULT_INITIAL_MAX_STREAM_DATA,
                DEFAULT_INITIAL_MAX_STREAM_DATA,
            ),
            recv_buf: HashMap::new(),
            recv_offset: 0,
            final_size: None,
            reset_error_code: None,
            send_offset: 0,
            fin_sent: false,
            acked_send_ranges: Vec::new(),
            fin_acked: false,
        }
    }

    #[derive(Debug, Default)]
    struct RecordingCongestionState {
        acked_packets: Vec<usize>,
        ce_packets: Vec<usize>,
        lost_packets: Vec<usize>,
        persistent_congestion_events: usize,
    }

    #[derive(Debug)]
    struct RecordingCongestionController {
        shared: Arc<Mutex<RecordingCongestionState>>,
        bytes_in_flight: usize,
    }

    impl RecordingCongestionController {
        fn new(shared: Arc<Mutex<RecordingCongestionState>>) -> Self {
            Self {
                shared,
                bytes_in_flight: 0,
            }
        }
    }

    impl CongestionController for RecordingCongestionController {
        fn new_instance(&self) -> Box<dyn CongestionController> {
            Box::new(RecordingCongestionController::new(Arc::new(Mutex::new(
                RecordingCongestionState::default(),
            ))))
        }

        fn on_packet_sent(&mut self, bytes: usize, _now: Instant) {
            self.bytes_in_flight += bytes;
        }

        fn on_ack(&mut self, bytes: usize, _rtt: Duration, _sent_time: Instant, _now: Instant) {
            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);
            self.shared.lock().unwrap().acked_packets.push(bytes);
        }

        fn on_loss(&mut self, bytes: usize, _sent_time: Instant, _now: Instant) {
            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);
            self.shared.lock().unwrap().lost_packets.push(bytes);
        }

        fn on_ecn_ce(&mut self, bytes: usize, _sent_time: Instant, _now: Instant) {
            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);
            self.shared.lock().unwrap().ce_packets.push(bytes);
        }

        fn on_packet_discarded(&mut self, bytes: usize) {
            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);
        }

        fn on_persistent_congestion(&mut self) {
            self.shared.lock().unwrap().persistent_congestion_events += 1;
        }

        fn window(&self) -> usize {
            usize::MAX
        }

        fn bytes_in_flight(&self) -> usize {
            self.bytes_in_flight
        }

        fn ssthresh(&self) -> usize {
            usize::MAX
        }
    }

    #[test]
    fn open_bidi_stream_returns_valid_id() {
        let mut conn = make_test_client();
        let stream_id = conn.open_bidi_stream().unwrap();
        // Client-initiated bidi streams have IDs 0, 4, 8, ... (RFC 9000 §2.1)
        assert_eq!(stream_id, 0);
        let stream_id2 = conn.open_bidi_stream().unwrap();
        assert_eq!(stream_id2, 4);
    }

    #[test]
    fn stream_send_buffers_data() {
        let mut conn = make_connected_test_client();
        let stream_id = conn.open_bidi_stream().unwrap();
        let n = conn.stream_send(stream_id, b"hello", false).unwrap();
        assert_eq!(n, 5);
    }

    #[test]
    fn stream_send_rejects_application_data_before_connection_ready() {
        let mut conn = make_test_client();
        let stream_id = conn.open_bidi_stream().unwrap();

        let err = conn.stream_send(stream_id, b"hello", false).unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::StreamStateError, _)
        ));
    }

    #[test]
    fn stream_send_on_receive_only_stream_errors() {
        let mut conn = make_test_client();
        let stream_id = 3; // server-initiated unidirectional
        conn.streams.accept_stream(stream_id).unwrap();

        let err = conn.stream_send(stream_id, b"hello", false).unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::StreamStateError, _)
        ));
    }

    #[test]
    fn stream_recv_on_empty_stream_returns_zero() {
        let mut conn = make_test_client();
        let stream_id = conn.open_bidi_stream().unwrap();
        let mut buf = [0u8; 64];
        let (n, fin) = conn.stream_recv(stream_id, &mut buf).unwrap();
        assert_eq!(n, 0);
        assert!(!fin);
    }

    #[test]
    fn stream_recv_on_send_only_stream_errors() {
        let mut conn = make_test_client();
        let stream_id = conn.open_uni_stream().unwrap();
        let mut buf = [0u8; 16];

        let err = conn.stream_recv(stream_id, &mut buf).unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::StreamStateError, _)
        ));
    }

    #[test]
    fn quic_stream_id_accessor() {
        let mut conn = make_test_client();
        let stream_id = conn.open_bidi_stream().unwrap();
        let stream = crate::stream::QuicStream::new(stream_id, &mut conn);
        assert_eq!(stream.stream_id(), stream_id);
    }

    #[test]
    fn connection_state_initial() {
        let conn = make_test_client();
        assert_eq!(conn.state(), ConnectionState::Initial);
        assert!(!conn.is_established());
    }

    #[test]
    fn validate_stream_final_size_rejects_changed_value() {
        let mut stream = make_stream_state();
        stream.final_size = Some(8);

        let err = QuicConnection::validate_stream_final_size(1, &stream, 9).unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::FinalSizeError, _)
        ));
    }

    #[test]
    fn validate_stream_final_size_rejects_smaller_than_received() {
        let mut stream = make_stream_state();
        assert_eq!(stream.flow_control.on_data_received(0, 10), 10);

        let err = QuicConnection::validate_stream_final_size(1, &stream, 9).unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::FinalSizeError, _)
        ));
    }

    #[test]
    fn ack_delay_overflow_does_not_panic() {
        let mut conn = make_test_client();
        // Set a large exponent that would cause overflow with << on large ack_delay
        conn.peer_ack_delay_exponent = 62;

        let ack = Frame::Ack(crate::frame::ack::AckFrame {
            largest_acknowledged: 0,
            ack_delay: u64::MAX,
            ranges: vec![crate::frame::ack::AckRange { start: 0, end: 0 }],
            ecn: None,
        });

        // Should not panic — checked_shl clamps to u64::MAX
        let result = conn.process_frame(PacketNumberSpace::Initial, ack);
        assert!(result.is_ok());
    }

    #[test]
    fn ack_delay_normal_exponent_scales_correctly() {
        let mut conn = make_test_client();
        conn.peer_ack_delay_exponent = 3; // default

        let ack = Frame::Ack(crate::frame::ack::AckFrame {
            largest_acknowledged: 0,
            ack_delay: 100, // 100 << 3 = 800 microseconds
            ranges: vec![crate::frame::ack::AckRange { start: 0, end: 0 }],
            ecn: None,
        });

        // Should process without error (no matching sent packets, but no panic)
        let result = conn.process_frame(PacketNumberSpace::Initial, ack);
        assert!(result.is_ok());
    }

    #[test]
    fn acking_fin_packet_waits_for_all_stream_data() {
        let mut conn = make_test_client();
        enable_application_data(&mut conn);

        let sid = conn.open_bidi_stream().unwrap();
        assert_eq!(conn.stream_send(sid, b"hello", false).unwrap(), 5);
        assert_eq!(conn.stream_send(sid, b" world", true).unwrap(), 6);

        let pending: Vec<Frame> = conn
            .pending_frames
            .drain(..)
            .map(|(_, frame)| frame)
            .collect();
        assert_eq!(pending.len(), 2);
        let mut data_frame = None;
        let mut fin_frame = None;
        for frame in pending {
            match frame {
                Frame::Stream(stream_frame) if stream_frame.fin => {
                    fin_frame = Some(Frame::Stream(stream_frame));
                }
                Frame::Stream(stream_frame) => {
                    data_frame = Some(Frame::Stream(stream_frame));
                }
                other => panic!("expected STREAM frame, got {other:?}"),
            }
        }
        let data_frame = data_frame.expect("missing non-FIN stream frame");
        let fin_frame = fin_frame.expect("missing FIN stream frame");

        // Capture `now` immediately before processing to simulate realistic timing:
        // in production, time_sent is recorded when packets are actually transmitted,
        // and ACKs arrive after network RTT - keeping the timing relationship consistent.
        conn.sent_frames[PacketNumberSpace::ApplicationData as usize].insert(0, vec![data_frame]);
        conn.sent_frames[PacketNumberSpace::ApplicationData as usize].insert(1, vec![fin_frame]);

        let now = Instant::now();
        conn.loss_detection.on_packet_sent(SentPacket {
            pn: 0,
            space: PacketNumberSpace::ApplicationData,
            time_sent: now,
            size: 100,
            ack_eliciting: true,
            ecn_marking: None,
        });
        conn.loss_detection.on_packet_sent(SentPacket {
            pn: 1,
            space: PacketNumberSpace::ApplicationData,
            time_sent: now,
            size: 200,
            ack_eliciting: true,
            ecn_marking: None,
        });
        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::Ack(crate::frame::ack::AckFrame {
                largest_acknowledged: 1,
                ack_delay: 0,
                ranges: vec![crate::frame::ack::AckRange { start: 1, end: 1 }],
                ecn: None,
            }),
        )
        .unwrap();

        assert_eq!(
            conn.streams.get(sid).unwrap().send,
            Some(SendState::DataSent)
        );

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::Ack(crate::frame::ack::AckFrame {
                largest_acknowledged: 1,
                ack_delay: 0,
                ranges: vec![crate::frame::ack::AckRange { start: 0, end: 1 }],
                ecn: None,
            }),
        )
        .unwrap();

        assert_eq!(
            conn.streams.get(sid).unwrap().send,
            Some(SendState::DataRecvd)
        );
    }

    #[test]
    fn ack_ecn_ce_only_penalizes_ecn_capable_packets() {
        let mut conn = make_test_client();
        enable_application_data(&mut conn);
        let shared = Arc::new(Mutex::new(RecordingCongestionState::default()));
        conn.congestion = Box::new(RecordingCongestionController::new(shared.clone()));

        let now = Instant::now();
        conn.loss_detection.on_packet_sent(SentPacket {
            pn: 0,
            space: PacketNumberSpace::ApplicationData,
            time_sent: now,
            size: 100,
            ack_eliciting: true,
            ecn_marking: None,
        });
        conn.loss_detection.on_packet_sent(SentPacket {
            pn: 1,
            space: PacketNumberSpace::ApplicationData,
            time_sent: now + Duration::from_millis(1),
            size: 200,
            ack_eliciting: true,
            ecn_marking: Some(UdpEcnCodepoint::Ect0),
        });
        conn.sent_ecn_counts[PacketNumberSpace::ApplicationData as usize] = EcnCounts {
            ect0: 1,
            ect1: 0,
            ce: 0,
        };

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::Ack(crate::frame::ack::AckFrame {
                largest_acknowledged: 1,
                ack_delay: 0,
                ranges: vec![crate::frame::ack::AckRange { start: 0, end: 1 }],
                ecn: Some(EcnCounts {
                    ect0: 0,
                    ect1: 0,
                    ce: 1,
                }),
            }),
        )
        .unwrap();

        let state = shared.lock().unwrap();
        assert_eq!(state.ce_packets, vec![200]);
        assert_eq!(state.acked_packets, vec![100]);
        assert!(state.lost_packets.is_empty());
    }

    #[test]
    fn persistent_congestion_signal_propagates_to_congestion_controller() {
        let mut conn = make_test_client();
        enable_application_data(&mut conn);
        let shared = Arc::new(Mutex::new(RecordingCongestionState::default()));
        conn.congestion = Box::new(RecordingCongestionController::new(shared.clone()));
        conn.loss_detection.rtt.smoothed_rtt = Duration::from_millis(10);
        conn.loss_detection.rtt.latest_rtt = Duration::from_millis(10);
        conn.loss_detection.rtt.rttvar = Duration::ZERO;
        conn.loss_detection.set_max_ack_delay(Duration::ZERO);

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
            conn.loss_detection.on_packet_sent(SentPacket {
                pn,
                space: PacketNumberSpace::ApplicationData,
                time_sent: base + Duration::from_millis(offset_ms),
                size: 1200,
                ack_eliciting: true,
                ecn_marking: None,
            });
        }

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::Ack(AckFrame {
                largest_acknowledged: 6,
                ack_delay: 0,
                ranges: vec![AckRange { start: 6, end: 6 }],
                ecn: None,
            }),
        )
        .unwrap();

        let state = shared.lock().unwrap();
        assert_eq!(state.persistent_congestion_events, 1);
    }

    #[test]
    fn first_packet_gap_requests_immediate_ack() {
        assert!(!QuicConnection::should_ack_immediately_for_packet(
            false, 0, 0, 1
        ));
        assert!(QuicConnection::should_ack_immediately_for_packet(
            false, 0, 1, 1
        ));
        assert!(QuicConnection::should_ack_immediately_for_packet(
            false, 0, 2, 1
        ));
        assert!(!QuicConnection::should_ack_immediately_for_packet(
            true, 4, 5, 1
        ));
        assert!(QuicConnection::should_ack_immediately_for_packet(
            true, 4, 6, 1
        ));
        assert!(QuicConnection::should_ack_immediately_for_packet(
            true, 4, 3, 1
        ));
        assert!(QuicConnection::should_ack_immediately_for_packet(
            true, 4, 5, 2
        ));
    }

    #[test]
    fn negotiated_idle_timeout_uses_minimum_non_zero_value() {
        assert_eq!(
            QuicConnection::negotiated_idle_timeout(0, 0),
            Duration::ZERO
        );
        assert_eq!(
            QuicConnection::negotiated_idle_timeout(30_000, 10_000),
            Duration::from_millis(10_000)
        );
        assert_eq!(
            QuicConnection::negotiated_idle_timeout(30_000, 0),
            Duration::from_millis(30_000)
        );
        assert_eq!(
            QuicConnection::negotiated_idle_timeout(0, 5_000),
            Duration::from_millis(5_000)
        );
    }

    #[test]
    fn encode_stateless_reset_builds_short_header_packet() {
        let token = [0xabu8; 16];
        let packet = QuicConnection::encode_stateless_reset(token, 1200).unwrap();

        assert_eq!(packet.len(), 1199);
        assert_eq!(packet[0] & 0x80, 0);
        assert_eq!(packet[0] & 0x40, 0x40);
        assert_eq!(&packet[packet.len() - 16..], &token);
    }

    #[test]
    fn generate_acks_delays_application_ack_until_timer_or_second_packet() {
        let mut conn = make_test_client();
        let provider = rustls::crypto::ring::default_provider();
        let keys =
            crate::crypto::keys::derive_initial_keys(&[1, 2, 3, 4, 5, 6, 7, 8], true, &provider)
                .unwrap();
        conn.keys
            .set_from_quic_keys(PacketNumberSpace::ApplicationData, keys);
        conn.pending_frames.clear();

        conn.ack_needed[PacketNumberSpace::ApplicationData as usize] = true;
        conn.ack_eliciting_packets_since_last_ack[PacketNumberSpace::ApplicationData as usize] = 1;
        conn.last_ack_eliciting_recv[PacketNumberSpace::ApplicationData as usize] =
            Some(Instant::now());
        conn.recv_pn_ranges[PacketNumberSpace::ApplicationData as usize] = vec![(0, 0)];

        conn.generate_acks();
        assert!(!conn
            .pending_frames
            .iter()
            .any(|(_, frame)| matches!(frame, Frame::Ack(_))));

        conn.ack_eliciting_packets_since_last_ack[PacketNumberSpace::ApplicationData as usize] = 2;
        conn.generate_acks();
        assert!(conn
            .pending_frames
            .iter()
            .any(|(_, frame)| matches!(frame, Frame::Ack(_))));

        conn.pending_frames.clear();
        conn.ack_needed[PacketNumberSpace::ApplicationData as usize] = true;
        conn.ack_eliciting_packets_since_last_ack[PacketNumberSpace::ApplicationData as usize] = 1;
        conn.last_ack_eliciting_recv[PacketNumberSpace::ApplicationData as usize] =
            Some(Instant::now() - conn.local_max_ack_delay - Duration::from_millis(1));
        conn.generate_acks();
        assert!(conn
            .pending_frames
            .iter()
            .any(|(_, frame)| matches!(frame, Frame::Ack(_))));
    }

    #[test]
    fn generated_ack_delay_uses_largest_received_packet_time() {
        let mut conn = make_connected_test_client();
        let now = Instant::now();

        conn.ack_needed[PacketNumberSpace::ApplicationData as usize] = true;
        conn.ack_eliciting_packets_since_last_ack[PacketNumberSpace::ApplicationData as usize] = 2;
        conn.last_ack_eliciting_recv[PacketNumberSpace::ApplicationData as usize] =
            Some(now - Duration::from_millis(1));
        conn.largest_recv_pn_time[PacketNumberSpace::ApplicationData as usize] =
            Some(now - Duration::from_millis(20));
        conn.recv_pn_ranges[PacketNumberSpace::ApplicationData as usize] = vec![(7, 9)];

        conn.generate_acks();

        let ack = conn
            .pending_frames
            .iter()
            .find_map(|(_, frame)| match frame {
                Frame::Ack(ack) => Some(ack),
                _ => None,
            })
            .expect("expected generated ACK");
        assert!(
            ack.ack_delay >= 1_000,
            "ack delay should reflect older largest packet arrival"
        );
    }

    #[test]
    fn generate_acks_includes_ecn_counts_when_present() {
        let mut conn = make_connected_test_client();
        let counts = EcnCounts {
            ect0: 3,
            ect1: 1,
            ce: 2,
        };

        conn.ack_needed[PacketNumberSpace::ApplicationData as usize] = true;
        conn.ack_eliciting_packets_since_last_ack[PacketNumberSpace::ApplicationData as usize] = 2;
        conn.last_ack_eliciting_recv[PacketNumberSpace::ApplicationData as usize] =
            Some(Instant::now());
        conn.recv_pn_ranges[PacketNumberSpace::ApplicationData as usize] = vec![(7, 9)];
        conn.recv_ecn_counts[PacketNumberSpace::ApplicationData as usize] = counts;

        conn.generate_acks();

        let ack = conn
            .pending_frames
            .iter()
            .find_map(|(_, frame)| match frame {
                Frame::Ack(ack) => Some(ack),
                _ => None,
            })
            .expect("expected generated ACK");
        assert_eq!(ack.ecn, Some(counts));
    }

    #[test]
    fn data_blocked_hint_requeues_capped_max_data() {
        let mut conn = make_connected_test_client();
        conn.flow_control.commit_max_data(1024);
        conn.pending_frames.clear();

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::DataBlocked {
                maximum_data: u64::MAX,
            },
        )
        .unwrap();

        assert!(conn.pending_frames.iter().any(|(_, frame)| {
            matches!(
                frame,
                Frame::MaxData { maximum_data }
                    if *maximum_data == 1024 + DEFAULT_INITIAL_MAX_DATA
            )
        }));
    }

    #[test]
    fn stream_data_blocked_hint_requeues_capped_max_stream_data() {
        let mut conn = make_connected_test_client();
        let sid = 1;
        conn.streams.accept_stream(sid).unwrap();
        conn.streams
            .get_mut(sid)
            .unwrap()
            .flow_control
            .commit_max_stream_data(2048);
        conn.pending_frames.clear();

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::StreamDataBlocked {
                stream_id: sid,
                maximum_stream_data: u64::MAX,
            },
        )
        .unwrap();

        assert!(conn.pending_frames.iter().any(|(_, frame)| {
            matches!(
                frame,
                Frame::MaxStreamData {
                    stream_id,
                    maximum_stream_data,
                } if *stream_id == sid
                    && *maximum_stream_data == 2048 + DEFAULT_INITIAL_MAX_STREAM_DATA
            )
        }));
    }

    #[test]
    fn streams_blocked_hint_requeues_current_max_streams() {
        let mut conn = make_connected_test_client();
        conn.pending_frames.clear();

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::StreamsBlocked {
                bidirectional: true,
                maximum_streams: DEFAULT_INITIAL_MAX_STREAMS_BIDI - 1,
            },
        )
        .unwrap();

        assert!(conn.pending_frames.iter().any(|(_, frame)| {
            matches!(
                frame,
                Frame::MaxStreams {
                    bidirectional: true,
                    maximum_streams,
                } if *maximum_streams == DEFAULT_INITIAL_MAX_STREAMS_BIDI
            )
        }));
    }

    #[test]
    fn application_data_uses_zero_rtt_before_one_rtt_keys() {
        let mut conn = make_test_client();
        enable_zero_rtt(&mut conn);
        conn.pending_frames.clear();

        let stream_id = conn.open_bidi_stream().unwrap();
        let written = conn.stream_send(stream_id, b"early", true).unwrap();
        assert_eq!(written, 5);
        assert!(conn.can_send_early_data());

        let frame = match conn.pending_frames.pop_front() {
            Some((PacketNumberSpace::ApplicationData, frame)) => frame,
            other => panic!("expected pending application frame, got {other:?}"),
        };
        let mut payload = Vec::new();
        frame.encode(&mut payload).unwrap();
        let packet = conn
            .build_packet_to_addr(
                PacketNumberSpace::ApplicationData,
                vec![frame],
                payload,
                conn.peer_addr,
                true,
                0,
            )
            .unwrap()
            .unwrap();

        match packet::header::parse_header(&packet.bytes, conn.cid_manager.cid_len() as usize)
            .unwrap()
        {
            PacketHeader::Long(h) => assert_eq!(h.packet_type, LongPacketType::ZeroRtt),
            other => panic!("expected 0-RTT long header, got {other:?}"),
        }
    }

    #[test]
    fn rejected_zero_rtt_frames_are_requeued_for_one_rtt() {
        let mut conn = make_test_client();
        enable_zero_rtt(&mut conn);
        conn.pending_frames.clear();

        let stream_id = conn.open_bidi_stream().unwrap();
        conn.stream_send(stream_id, b"retry-me", true).unwrap();
        let frame = match conn.pending_frames.pop_front() {
            Some((PacketNumberSpace::ApplicationData, frame)) => frame,
            other => panic!("expected pending application frame, got {other:?}"),
        };

        conn.sent_frames[PacketNumberSpace::ApplicationData as usize].insert(7, vec![frame]);
        conn.loss_detection
            .on_packet_sent(crate::recovery::loss_detection::SentPacket {
                pn: 7,
                space: PacketNumberSpace::ApplicationData,
                time_sent: Instant::now(),
                size: 128,
                ack_eliciting: true,
                ecn_marking: Some(UdpEcnCodepoint::Ect0),
            });
        conn.congestion.on_packet_sent(128, Instant::now());
        conn.zero_rtt_in_flight.insert(7);
        conn.attempted_zero_rtt = true;

        enable_application_data(&mut conn);
        conn.finalize_client_early_data_state();

        assert_eq!(conn.early_data_accepted(), Some(false));
        assert!(conn.zero_rtt_in_flight.is_empty());
        match conn.pending_frames.pop_front() {
            Some((PacketNumberSpace::ApplicationData, Frame::Stream(sf))) => {
                assert_eq!(sf.stream_id, stream_id);
                assert_eq!(sf.data, b"retry-me");
                assert!(sf.fin);
            }
            other => panic!("expected retransmitted STREAM frame, got {other:?}"),
        }
    }

    #[test]
    fn ack_ecn_ce_feedback_reduces_congestion_window() {
        let mut conn = make_connected_test_client();
        let packet = arm_path_mtu_probe(&mut conn);
        let initial_window = conn.congestion.window();

        let ack = Frame::Ack(crate::frame::ack::AckFrame {
            largest_acknowledged: packet.pn,
            ack_delay: 0,
            ranges: vec![crate::frame::ack::AckRange {
                start: packet.pn,
                end: packet.pn,
            }],
            ecn: Some(EcnCounts {
                ect0: 0,
                ect1: 0,
                ce: 1,
            }),
        });

        conn.process_frame(PacketNumberSpace::ApplicationData, ack)
            .unwrap();

        assert_eq!(
            conn.peer_ack_ecn_counts[PacketNumberSpace::ApplicationData as usize],
            Some(EcnCounts {
                ect0: 0,
                ect1: 0,
                ce: 1,
            })
        );
        assert!(conn.congestion.window() < initial_window);
    }

    #[test]
    fn missing_ack_ecn_feedback_disables_outbound_ecn() {
        let mut conn = make_connected_test_client();
        let packet = arm_path_mtu_probe(&mut conn);

        let ack = Frame::Ack(crate::frame::ack::AckFrame {
            largest_acknowledged: packet.pn,
            ack_delay: 0,
            ranges: vec![crate::frame::ack::AckRange {
                start: packet.pn,
                end: packet.pn,
            }],
            ecn: None,
        });

        conn.process_frame(PacketNumberSpace::ApplicationData, ack)
            .unwrap();

        assert!(!conn.outbound_ecn_enabled);
        assert_eq!(
            conn.peer_ack_ecn_counts[PacketNumberSpace::ApplicationData as usize],
            None
        );
    }

    #[test]
    fn local_close_before_one_rtt_uses_transport_close_in_initial_space() {
        let conn = make_test_client();
        let close_frames = conn.local_close_frames(0x42, b"app-close");

        assert_eq!(close_frames.len(), 1);
        match &close_frames[0] {
            (
                PacketNumberSpace::Initial,
                Frame::ConnectionClose {
                    is_transport,
                    error_code,
                    frame_type,
                    reason,
                },
            ) => {
                assert!(*is_transport);
                assert_eq!(*error_code, TransportErrorCode::ApplicationError.to_u64());
                assert_eq!(*frame_type, Some(0));
                assert!(reason.is_empty());
            }
            other => panic!("expected Initial transport close, got {other:?}"),
        }
    }

    #[test]
    fn local_close_with_one_rtt_keeps_app_close_and_handshake_fallback() {
        let mut conn = make_test_client();
        enable_handshake_data(&mut conn);
        enable_application_data(&mut conn);

        let close_frames = conn.local_close_frames(0x33, b"bye");

        assert!(close_frames.iter().any(|(space, frame)| {
            *space == PacketNumberSpace::Handshake
                && matches!(
                    frame,
                    Frame::ConnectionClose {
                        is_transport: true,
                        error_code,
                        frame_type: Some(0),
                        reason,
                    } if *error_code == TransportErrorCode::ApplicationError.to_u64()
                        && reason.is_empty()
                )
        }));
        assert!(close_frames.iter().any(|(space, frame)| {
            *space == PacketNumberSpace::ApplicationData
                && matches!(
                    frame,
                    Frame::ConnectionClose {
                        is_transport: false,
                        error_code: 0x33,
                        frame_type: None,
                        reason,
                    } if reason == b"bye"
                )
        }));
    }

    #[test]
    fn local_close_bypasses_congestion_window_accounting() {
        let mut reactor = Reactor::new();

        let task = reactor.spawn_with_result(async move {
            let mut conn = make_connected_test_client();
            let bytes_in_flight = conn.congestion.window();
            conn.congestion
                .on_packet_sent(bytes_in_flight, Instant::now());
            let pn_before = conn.next_pn;

            conn.close(0x44, b"busy").await.map_err(|e| e.to_string())?;

            assert_eq!(conn.state(), ConnectionState::Closing);
            assert!(!conn.closing_response_pending);
            assert!(conn
                .next_pn
                .iter()
                .zip(pn_before.iter())
                .any(|(after, before)| after > before));
            assert_eq!(conn.congestion.bytes_in_flight(), bytes_in_flight);
            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }

    #[test]
    fn local_close_stays_pending_when_anti_amplification_limited() {
        let mut reactor = Reactor::new();

        let task = reactor.spawn_with_result(async move {
            let mut conn = make_connected_test_client();
            conn.side = Side::Server;
            conn.peer_address_validated = false;
            conn.bytes_received = 0;
            conn.bytes_sent = 0;
            conn.store_active_peer_path_state();
            let pn_before = conn.next_pn;

            conn.close(0x45, b"amp").await.map_err(|e| e.to_string())?;

            assert_eq!(conn.state(), ConnectionState::Closing);
            assert!(conn.closing_response_pending);
            assert_eq!(conn.next_pn, pn_before);
            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }

    #[test]
    fn begin_local_close_enters_closing_and_peer_close_resets_deadline() {
        let mut conn = make_connected_test_client();
        let now = Instant::now();
        let later = now + Duration::from_millis(50);
        let close_frames = conn.local_close_frames(0x11, b"bye");

        conn.begin_local_close(close_frames, now);

        assert_eq!(conn.state(), ConnectionState::Closing);
        let closing_deadline = conn.draining_until.expect("closing deadline");
        conn.enter_draining_at(later);

        assert_eq!(conn.state(), ConnectionState::Draining);
        assert_eq!(
            conn.draining_until,
            Some(later + conn.loss_detection.pto_timeout() * 3)
        );
        assert!(conn.draining_until.expect("draining deadline") > closing_deadline);
    }

    #[test]
    fn draining_state_closes_when_deadline_expires() {
        let mut conn = make_connected_test_client();
        conn.enter_draining();

        let deadline = conn.draining_until.expect("draining deadline");
        conn.handle_internal_timers(deadline - Duration::from_millis(1));
        assert_eq!(conn.state(), ConnectionState::Draining);

        conn.handle_internal_timers(deadline + Duration::from_millis(1));
        assert_eq!(conn.state(), ConnectionState::Closed);
    }

    #[test]
    fn anti_amplification_caps_unvalidated_server_to_three_times_bytes_received() {
        let mut conn = make_test_client();
        conn.side = Side::Server;
        conn.peer_address_validated = false;
        conn.bytes_received = 100;
        conn.bytes_sent = 299;
        conn.store_active_peer_path_state();

        assert!(conn.can_send_to_addr(conn.peer_addr, 1));
        assert!(!conn.can_send_to_addr(conn.peer_addr, 2));

        conn.peer_address_validated = true;
        conn.store_active_peer_path_state();
        assert!(conn.can_send_to_addr(conn.peer_addr, 1_000));
    }

    #[test]
    fn anti_amplification_budget_is_tracked_per_path() {
        let mut conn = make_test_client();
        conn.side = Side::Server;
        conn.peer_address_validated = false;
        conn.bytes_received = 100;
        conn.bytes_sent = 0;
        conn.store_active_peer_path_state();

        let alt_addr: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        assert!(!conn.can_send_to_addr(alt_addr, 1));

        conn.record_unvalidated_bytes_received(alt_addr, 50);

        assert!(conn.can_send_to_addr(conn.peer_addr, 300));
        assert!(!conn.can_send_to_addr(conn.peer_addr, 301));
        assert!(conn.can_send_to_addr(alt_addr, 150));
        assert!(!conn.can_send_to_addr(alt_addr, 151));
    }

    #[test]
    fn validating_one_path_does_not_lift_other_paths_budget() {
        let mut conn = make_test_client();
        conn.side = Side::Server;
        conn.peer_address_validated = false;
        conn.bytes_received = 0;
        conn.bytes_sent = 0;
        conn.store_active_peer_path_state();

        let alt_addr: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        conn.record_unvalidated_bytes_received(alt_addr, 10);
        conn.mark_peer_path_validated(alt_addr);

        assert!(conn.can_send_to_addr(alt_addr, 10_000));
        assert!(!conn.peer_address_validated);
        assert!(!conn.can_send_to_addr(conn.peer_addr, 1));
    }

    #[test]
    fn non_initial_packet_on_alternate_path_does_not_auto_validate_it() {
        let mut conn = make_test_client();
        conn.side = Side::Server;
        conn.state = ConnectionState::Connected;
        conn.peer_address_validated = false;
        conn.bytes_received = 1;
        conn.bytes_sent = 0;
        conn.store_active_peer_path_state();

        let alt_addr: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        conn.record_unvalidated_bytes_received(alt_addr, 50);

        assert!(!conn.should_auto_validate_peer_path(PacketNumberSpace::Handshake, alt_addr));
        assert!(!conn.peer_path_state_for_addr(alt_addr).validated);
        assert!(conn.can_send_to_addr(alt_addr, 150));
        assert!(!conn.can_send_to_addr(alt_addr, 151));
    }

    #[test]
    fn closing_connection_retransmits_close_after_subsequent_datagram() {
        let mut reactor = Reactor::new();

        let task = reactor.spawn_with_result(async move {
            let mut conn = make_connected_test_client();
            let close_frames = conn.local_close_frames(0x55, b"done");
            let pn_before = conn.next_pn;

            conn.begin_local_close(close_frames, Instant::now());
            conn.feed_datagram_from(b"ignored", conn.peer_addr)
                .map_err(|e| e.to_string())?;
            assert!(conn.closing_response_pending);

            conn.flush().await.map_err(|e| e.to_string())?;

            assert!(!conn.closing_response_pending);
            assert!(conn
                .next_pn
                .iter()
                .zip(pn_before.iter())
                .any(|(after, before)| after > before));
            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }
    #[test]
    fn poll_with_timeout_does_not_send_after_queued_datagram_enters_draining() {
        let mut reactor = Reactor::new();

        let task = reactor.spawn_with_result(async move {
            let mut conn = make_connected_test_client();
            conn.endpoint_managed = true;

            let token = [0xAB; 16];
            let peer_cid = ConnectionId::from_slice(&[9, 10, 11, 12]);
            conn.cid_manager
                .add_peer_cid(1, peer_cid, token, 0)
                .unwrap();

            let mut datagram = vec![0x40u8];
            datagram.extend_from_slice(&[0u8; 20]);
            datagram.extend_from_slice(&token);
            conn.incoming_datagrams.push_back(datagram);
            conn.pending_frames
                .push_back((PacketNumberSpace::ApplicationData, Frame::Ping));

            let pn_before = conn.next_pn;
            conn.poll_with_timeout(&None)
                .await
                .map_err(|e| e.to_string())?;

            assert_eq!(conn.state(), ConnectionState::Draining);
            assert_eq!(conn.next_pn, pn_before);
            assert!(conn
                .pending_frames
                .iter()
                .any(|(_, frame)| matches!(frame, Frame::Ping)));
            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }

    #[test]
    fn first_ack_eliciting_send_restarts_idle_timeout_once() {
        let mut conn = make_connected_test_client();
        conn.idle_timeout = Duration::from_secs(10);

        let recv_time = Instant::now() - Duration::from_secs(8);
        conn.record_packet_received(recv_time);

        let first_send = recv_time + Duration::from_secs(3);
        conn.record_packet_sent(first_send, true);
        assert_eq!(conn.idle_deadline(), Some(first_send + conn.idle_timeout));

        let second_send = first_send + Duration::from_secs(3);
        conn.record_packet_sent(second_send, true);
        assert_eq!(conn.idle_deadline(), Some(first_send + conn.idle_timeout));

        conn.handle_internal_timers(first_send + conn.idle_timeout - Duration::from_millis(1));
        assert_eq!(conn.state(), ConnectionState::Connected);

        conn.handle_internal_timers(first_send + conn.idle_timeout + Duration::from_millis(1));
        assert_eq!(conn.state(), ConnectionState::Closed);
    }

    #[test]
    fn build_path_mtu_probe_packet_hits_requested_size() {
        let mut conn = make_connected_test_client();
        let target_size = conn.next_path_mtu_probe_size().unwrap();

        let packet = conn
            .build_path_mtu_probe_packet(target_size)
            .unwrap()
            .unwrap();

        assert_eq!(packet.space, PacketNumberSpace::ApplicationData);
        assert_eq!(packet.bytes.len(), target_size);
        assert!(packet.bytes.len() > MAX_DATAGRAM_SIZE);
        assert!(matches!(packet.frames.as_slice(), [Frame::Ping]));
    }

    #[test]
    fn path_mtu_probe_ack_increases_send_limit() {
        let mut conn = make_connected_test_client();
        let packet = arm_path_mtu_probe(&mut conn);

        let ack = Frame::Ack(crate::frame::ack::AckFrame {
            largest_acknowledged: packet.pn,
            ack_delay: 0,
            ranges: vec![crate::frame::ack::AckRange {
                start: packet.pn,
                end: packet.pn,
            }],
            ecn: None,
        });

        conn.process_frame(PacketNumberSpace::ApplicationData, ack)
            .unwrap();

        assert_eq!(conn.max_send_udp_payload_size, packet.bytes.len());
        assert!(conn.max_send_udp_payload_size > MAX_DATAGRAM_SIZE);
        assert!(conn.path_mtu.in_flight_probe.is_none());
        assert!(conn.path_mtu.next_probe_at.is_some());
    }

    #[test]
    fn path_mtu_probe_timeout_reduces_upper_bound_and_reschedules() {
        let mut conn = make_connected_test_client();
        let packet = arm_path_mtu_probe(&mut conn);
        let failed_size = packet.bytes.len();

        if let Some(probe) = conn.path_mtu.in_flight_probe.as_mut() {
            probe.sent_at =
                Instant::now() - conn.loss_detection.pto_timeout() - Duration::from_millis(1);
        }

        conn.handle_path_mtu_probe_timeout(Instant::now());

        assert!(conn.path_mtu.in_flight_probe.is_none());
        assert_eq!(conn.max_send_udp_payload_size, MAX_DATAGRAM_SIZE);
        assert_eq!(conn.path_mtu.probe_upper_bound, failed_size - 1);
        assert!(conn.path_mtu.next_probe_at.is_some());
        assert!(conn.next_path_mtu_probe_size().unwrap() < failed_size);
    }

    #[test]
    fn smaller_peer_limit_abandons_in_flight_path_mtu_probe_without_loss() {
        let mut conn = make_connected_test_client();
        let packet = arm_path_mtu_probe(&mut conn);
        let initial_window = conn.congestion.window();
        let initial_upper_bound = conn.path_mtu.probe_upper_bound;

        let params = packet::PeerTransportParameters {
            max_idle_timeout_ms: DEFAULT_IDLE_TIMEOUT_MS,
            max_udp_payload_size: MAX_DATAGRAM_SIZE as u64,
            initial_max_data: DEFAULT_INITIAL_MAX_DATA,
            initial_max_stream_data_bidi_local: DEFAULT_INITIAL_MAX_STREAM_DATA,
            initial_max_stream_data_bidi_remote: DEFAULT_INITIAL_MAX_STREAM_DATA,
            initial_max_stream_data_uni: DEFAULT_INITIAL_MAX_STREAM_DATA,
            initial_max_streams_bidi: DEFAULT_INITIAL_MAX_STREAMS_BIDI,
            initial_max_streams_uni: DEFAULT_INITIAL_MAX_STREAMS_UNI,
            ack_delay_exponent: DEFAULT_ACK_DELAY_EXPONENT,
            max_ack_delay_ms: packet::DEFAULT_MAX_ACK_DELAY_MS,
            active_connection_id_limit: DEFAULT_ACTIVE_CONNECTION_ID_LIMIT,
            ..Default::default()
        };

        conn.apply_peer_transport_parameter_values(&params);

        assert!(conn.path_mtu.in_flight_probe.is_none());
        assert_eq!(conn.congestion.window(), initial_window);
        assert_eq!(conn.path_mtu.probe_upper_bound, initial_upper_bound);
        assert!(!conn
            .loss_detection
            .has_unacked(PacketNumberSpace::ApplicationData));
        assert!(
            !conn.sent_frames[PacketNumberSpace::ApplicationData as usize].contains_key(&packet.pn)
        );
    }

    #[test]
    fn remembered_resumption_parameters_only_apply_zero_rtt_send_limits() {
        let mut conn = make_test_client();
        let cache: PeerTransportParameterCache = Arc::new(Mutex::new(HashMap::new()));
        let cache_key = "localhost:h3".to_string();
        let params = packet::PeerTransportParameters {
            max_idle_timeout_ms: 1,
            max_udp_payload_size: MAX_LOCAL_UDP_PAYLOAD_SIZE as u64,
            initial_max_data: 1234,
            initial_max_stream_data_bidi_local: 111,
            initial_max_stream_data_bidi_remote: 222,
            initial_max_stream_data_uni: 333,
            initial_max_streams_bidi: 1,
            initial_max_streams_uni: 0,
            ack_delay_exponent: 5,
            max_ack_delay_ms: 99,
            active_connection_id_limit: 9,
            stateless_reset_token: Some([0xabu8; 16]),
            ..Default::default()
        };
        cache
            .lock()
            .unwrap()
            .insert(cache_key.clone(), params.clone());

        let token_cache: ClientTokenCache = Arc::new(Mutex::new(HashMap::new()));
        conn.configure_client_resumption_state(cache_key, cache, token_cache);

        assert_eq!(conn.flow_control.send_window(), 1234);
        assert!(conn.open_bidi_stream().is_ok());
        assert!(matches!(
            conn.open_bidi_stream(),
            Err(QuicError::Transport(
                TransportErrorCode::StreamLimitError,
                _
            ))
        ));
        assert_eq!(
            conn.idle_timeout,
            Duration::from_millis(DEFAULT_IDLE_TIMEOUT_MS)
        );
        assert_eq!(conn.peer_ack_delay_exponent, DEFAULT_ACK_DELAY_EXPONENT);
        assert_eq!(
            conn.peer_max_ack_delay,
            Duration::from_millis(packet::DEFAULT_MAX_ACK_DELAY_MS)
        );
        assert_eq!(conn.path_mtu.peer_max_udp_payload_size, MAX_DATAGRAM_SIZE);
        assert_eq!(
            conn.peer_active_connection_id_limit,
            DEFAULT_ACTIVE_CONNECTION_ID_LIMIT
        );
        assert!(!conn.peer_transport_parameters_applied);
    }

    #[test]
    fn configure_client_resumption_state_loads_and_consumes_cached_new_token() {
        let mut conn = make_test_client();
        let cache: PeerTransportParameterCache = Arc::new(Mutex::new(HashMap::new()));
        let token_cache: ClientTokenCache = Arc::new(Mutex::new(HashMap::new()));
        let cache_key = "localhost:h3".to_string();
        let token = vec![0xde, 0xad, 0xbe, 0xef];
        token_cache
            .lock()
            .unwrap()
            .insert(cache_key.clone(), token.clone());

        conn.configure_client_resumption_state(cache_key.clone(), cache, token_cache.clone());

        assert_eq!(conn.retry_token, token);
        assert!(!token_cache.lock().unwrap().contains_key(&cache_key));
    }

    #[test]
    fn validate_peer_transport_parameters_accepts_expected_server_cids() {
        let original_dcid = ConnectionId::from_slice(&[0x10, 0x20]);
        let peer_initial_source_connection_id = ConnectionId::from_slice(&[0x44, 0x55]);
        let retry_source_connection_id = ConnectionId::from_slice(&[0x66, 0x77]);

        let params = packet::PeerTransportParameters {
            original_destination_connection_id: Some(original_dcid.clone()),
            initial_source_connection_id: Some(ConnectionId::from_slice(&[0x44, 0x55])),
            retry_source_connection_id: Some(ConnectionId::from_slice(&[0x66, 0x77])),
            ..Default::default()
        };

        assert!(
            QuicConnection::validate_peer_transport_parameters_for_state(
                Side::Client,
                &original_dcid,
                Some(&peer_initial_source_connection_id),
                Some(&retry_source_connection_id),
                &params,
            )
            .is_ok()
        );
    }

    #[test]
    fn validate_peer_transport_parameters_rejects_large_ack_delay_exponent() {
        let original_dcid = ConnectionId::from_slice(&[0x10, 0x20]);
        let peer_initial_source_connection_id = ConnectionId::from_slice(&[0x44, 0x55]);

        let params = packet::PeerTransportParameters {
            original_destination_connection_id: Some(original_dcid.clone()),
            initial_source_connection_id: Some(peer_initial_source_connection_id.clone()),
            ack_delay_exponent: packet::MAX_ACK_DELAY_EXPONENT + 1,
            ..Default::default()
        };

        let err = QuicConnection::validate_peer_transport_parameters_for_state(
            Side::Client,
            &original_dcid,
            Some(&peer_initial_source_connection_id),
            None,
            &params,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::TransportParameterError, _)
        ));
    }

    #[test]
    fn validate_peer_transport_parameters_rejects_missing_initial_source_cid() {
        let original_dcid = ConnectionId::from_slice(&[0x10, 0x20]);
        let peer_initial_source_connection_id = ConnectionId::from_slice(&[0x44, 0x55]);

        let params = packet::PeerTransportParameters {
            original_destination_connection_id: Some(original_dcid.clone()),
            ..Default::default()
        };

        let err = QuicConnection::validate_peer_transport_parameters_for_state(
            Side::Client,
            &original_dcid,
            Some(&peer_initial_source_connection_id),
            None,
            &params,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::TransportParameterError, _)
        ));
    }

    #[test]
    fn begin_peer_path_validation_queues_path_challenge() {
        let mut conn = make_test_client();
        conn.state = ConnectionState::Connected;
        let new_addr: SocketAddr = "127.0.0.1:4444".parse().unwrap();

        conn.begin_peer_path_validation(new_addr);

        let validation = conn.pending_peer_path_validation.clone().unwrap();
        assert_eq!(validation.peer_addr, new_addr);
        assert_eq!(validation.kind, PathValidationKind::ObservedPeerMigration);
        assert_eq!(conn.pending_targeted_frames.len(), 1);
        match conn.pending_targeted_frames.front() {
            Some((addr, PacketNumberSpace::ApplicationData, Frame::PathChallenge { data })) => {
                assert_eq!(*addr, new_addr);
                assert_eq!(*data, validation.challenge_data);
            }
            other => panic!("expected queued PATH_CHALLENGE, got {other:?}"),
        }
    }

    #[test]
    fn disable_active_migration_blocks_observed_peer_path_validation() {
        let mut conn = make_connected_test_client();
        let new_addr: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        let params = packet::PeerTransportParameters {
            disable_active_migration: true,
            ..Default::default()
        };

        conn.apply_peer_transport_parameter_values(&params);
        conn.begin_peer_path_validation(new_addr);

        assert!(conn.pending_peer_path_validation.is_none());
        assert!(conn.pending_targeted_frames.is_empty());
    }

    #[test]
    fn preferred_address_queues_validation_even_when_active_migration_is_disabled() {
        let mut conn = make_connected_test_client();
        let preferred_addr: SocketAddr = "127.0.0.1:5444".parse().unwrap();
        let preferred_cid = [0x90, 0x91, 0x92, 0x93];
        let preferred_token = [0x55; 16];
        let params = packet::PeerTransportParameters {
            disable_active_migration: true,
            preferred_address: Some(build_preferred_address_bytes(
                preferred_addr,
                &preferred_cid,
                preferred_token,
            )),
            ..Default::default()
        };

        conn.apply_peer_transport_parameter_values(&params);

        let validation = conn
            .pending_peer_path_validation
            .clone()
            .expect("preferred-address validation");
        assert_eq!(validation.peer_addr, preferred_addr);
        assert!(matches!(
            validation.kind,
            PathValidationKind::PreferredAddress(_)
        ));
        match conn.pending_targeted_frames.front() {
            Some((addr, PacketNumberSpace::ApplicationData, Frame::PathChallenge { data })) => {
                assert_eq!(*addr, preferred_addr);
                assert_eq!(*data, validation.challenge_data);
            }
            other => panic!("expected preferred-address PATH_CHALLENGE, got {other:?}"),
        }

        let frame = Frame::PathChallenge {
            data: validation.challenge_data,
        };
        let mut payload = Vec::new();
        frame.encode(&mut payload).unwrap();
        let packet = conn
            .build_packet_to_addr(
                PacketNumberSpace::ApplicationData,
                vec![frame],
                payload,
                preferred_addr,
                false,
                0,
            )
            .unwrap()
            .unwrap();
        let header =
            crate::packet::header::parse_header(&packet.bytes, preferred_cid.len()).unwrap();
        match header {
            PacketHeader::Short(short) => {
                assert_eq!(short.dcid.as_slice(), preferred_cid);
            }
            other => panic!("expected short header, got {other:?}"),
        }
    }

    #[test]
    fn matching_path_response_updates_active_peer_addr() {
        let mut conn = make_test_client();
        conn.state = ConnectionState::Connected;
        let new_addr: SocketAddr = "127.0.0.1:4444".parse().unwrap();

        conn.begin_peer_path_validation(new_addr);
        let challenge_data = conn
            .pending_peer_path_validation
            .as_ref()
            .unwrap()
            .challenge_data;

        conn.process_frame_from(
            PacketNumberSpace::ApplicationData,
            ReceivedPacketKind::OneRtt,
            Frame::PathResponse {
                data: challenge_data,
            },
            new_addr,
        )
        .unwrap();

        assert_eq!(conn.peer_addr, new_addr);
        assert!(conn.pending_peer_path_validation.is_none());
    }

    #[test]
    fn successful_path_validation_rotates_peer_cid_and_resets_recovery_state() {
        let mut conn = make_connected_test_client();
        let old_addr = conn.peer_addr;
        let new_addr: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        let old_peer_cid = conn.cid_manager.active_peer_cid().clone();
        let replacement_peer_cid = ConnectionId::from_slice(&[0x99, 0x98, 0x97, 0x96]);
        conn.cid_manager
            .add_peer_cid(1, replacement_peer_cid.clone(), [0xaa; 16], 0)
            .unwrap();

        let sent_at = Instant::now() - Duration::from_millis(5);
        conn.loss_detection.on_packet_sent(SentPacket {
            pn: 7,
            space: PacketNumberSpace::ApplicationData,
            time_sent: sent_at,
            size: 1200,
            ack_eliciting: true,
            ecn_marking: None,
        });
        conn.sent_frames[PacketNumberSpace::ApplicationData as usize].insert(
            7,
            vec![Frame::MaxData {
                maximum_data: DEFAULT_INITIAL_MAX_DATA + 1,
            }],
        );
        conn.congestion.on_packet_sent(1200, sent_at);
        conn.congestion
            .on_loss(1200, sent_at, sent_at + Duration::from_millis(1));
        assert!(conn.congestion.window() < INITIAL_WINDOW);

        conn.begin_peer_path_validation(new_addr);
        let challenge_data = conn
            .pending_peer_path_validation
            .as_ref()
            .unwrap()
            .challenge_data;

        conn.process_frame_from(
            PacketNumberSpace::ApplicationData,
            ReceivedPacketKind::OneRtt,
            Frame::PathResponse {
                data: challenge_data,
            },
            new_addr,
        )
        .unwrap();

        assert_eq!(conn.peer_addr, new_addr);
        assert_ne!(conn.peer_addr, old_addr);
        assert_eq!(
            conn.cid_manager.active_peer_cid().as_slice(),
            replacement_peer_cid.as_slice()
        );
        assert_ne!(
            conn.cid_manager.active_peer_cid().as_slice(),
            old_peer_cid.as_slice()
        );
        assert_eq!(conn.congestion.window(), INITIAL_WINDOW);
        assert_eq!(conn.congestion.bytes_in_flight(), 0);
        assert!(!conn
            .loss_detection
            .has_unacked(PacketNumberSpace::ApplicationData));
        assert!(conn.pending_frames.iter().any(|(space, frame)| *space
            == PacketNumberSpace::ApplicationData
            && matches!(frame, Frame::MaxData { .. })));
    }

    #[test]
    fn mismatched_path_response_keeps_old_peer_addr() {
        let mut conn = make_test_client();
        conn.state = ConnectionState::Connected;
        let old_addr = conn.peer_addr;
        let new_addr: SocketAddr = "127.0.0.1:4444".parse().unwrap();

        conn.begin_peer_path_validation(new_addr);
        conn.process_frame_from(
            PacketNumberSpace::ApplicationData,
            ReceivedPacketKind::OneRtt,
            Frame::PathResponse { data: [0u8; 8] },
            new_addr,
        )
        .unwrap();

        assert_eq!(conn.peer_addr, old_addr);
        assert!(conn.pending_peer_path_validation.is_some());
    }

    #[test]
    fn preferred_address_path_response_switches_peer_cid_and_token() {
        let mut conn = make_connected_test_client();
        let preferred_addr: SocketAddr = "127.0.0.1:5444".parse().unwrap();
        let preferred_cid = [0xa0, 0xa1, 0xa2, 0xa3];
        let preferred_token = [0x77; 16];
        let params = packet::PeerTransportParameters {
            preferred_address: Some(build_preferred_address_bytes(
                preferred_addr,
                &preferred_cid,
                preferred_token,
            )),
            ..Default::default()
        };

        conn.apply_peer_transport_parameter_values(&params);
        let challenge_data = conn
            .pending_peer_path_validation
            .clone()
            .unwrap()
            .challenge_data;

        conn.process_frame_from(
            PacketNumberSpace::ApplicationData,
            ReceivedPacketKind::OneRtt,
            Frame::PathResponse {
                data: challenge_data,
            },
            preferred_addr,
        )
        .unwrap();

        assert_eq!(conn.peer_addr, preferred_addr);
        assert_eq!(conn.cid_manager.active_peer_cid().as_slice(), preferred_cid);
        assert!(conn
            .cid_manager
            .is_peer_stateless_reset_token(&preferred_token));
        assert!(conn.peer_preferred_address.is_none());
        assert!(conn.pending_peer_path_validation.is_none());
    }

    #[test]
    fn preferred_address_validation_waits_for_existing_migration_probe() {
        let mut conn = make_connected_test_client();
        let migration_addr: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        let preferred_addr: SocketAddr = "127.0.0.1:5444".parse().unwrap();
        let preferred_cid = [0xa0, 0xa1, 0xa2, 0xa3];
        let preferred_token = [0x77; 16];

        conn.begin_peer_path_validation(migration_addr);
        let observed_validation = conn.pending_peer_path_validation.clone().unwrap();

        conn.apply_peer_transport_parameter_values(&packet::PeerTransportParameters {
            preferred_address: Some(build_preferred_address_bytes(
                preferred_addr,
                &preferred_cid,
                preferred_token,
            )),
            ..Default::default()
        });

        let in_flight = conn.pending_peer_path_validation.clone().unwrap();
        assert_eq!(in_flight.peer_addr, migration_addr);
        assert_eq!(in_flight.challenge_data, observed_validation.challenge_data);
        assert!(matches!(
            in_flight.kind,
            PathValidationKind::ObservedPeerMigration
        ));

        conn.process_frame_from(
            PacketNumberSpace::ApplicationData,
            ReceivedPacketKind::OneRtt,
            Frame::PathResponse {
                data: observed_validation.challenge_data,
            },
            migration_addr,
        )
        .unwrap();

        assert_eq!(conn.peer_addr, migration_addr);
        let preferred_validation = conn.pending_peer_path_validation.clone().unwrap();
        assert_eq!(preferred_validation.peer_addr, preferred_addr);
        assert!(matches!(
            preferred_validation.kind,
            PathValidationKind::PreferredAddress(_)
        ));
    }

    #[test]
    fn initiate_key_update_rotates_local_phase_while_preserving_old_read_phase() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut reactor = Reactor::new();
        let (server_config, client_config) = make_quic_configs();
        let observed = Arc::new(Mutex::new((false, false, false)));
        let addr_slot: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));

        let server_slot = Arc::clone(&addr_slot);
        let server = reactor.spawn_with_result(async move {
            let mut listener = QuicListener::bind("127.0.0.1:0".parse().unwrap(), server_config)
                .await
                .map_err(|e| e.to_string())?;
            *server_slot.lock().unwrap() = Some(listener.local_addr().map_err(|e| e.to_string())?);
            let _conn = listener.accept().await.map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        });

        let client_slot = Arc::clone(&addr_slot);
        let client_observed = Arc::clone(&observed);
        let client = reactor.spawn_with_result(async move {
            let deadline = Instant::now() + Duration::from_secs(1);
            let addr = loop {
                if let Some(addr) = *client_slot.lock().unwrap() {
                    break addr;
                }
                if Instant::now() >= deadline {
                    return Err("timed out waiting for server bind".into());
                }
                cooperative_sleep(Duration::from_millis(1)).await;
            };
            let mut conn = QuicClient::connect(addr, "localhost", client_config)
                .await
                .map_err(|e| e.to_string())?;

            assert!(!conn.local_key_phase);
            assert!(!conn.remote_key_phase);
            assert!(conn.pending_remote_key_update.is_none());

            if !conn.initiate_key_update().map_err(|e| e.to_string())? {
                return Err("expected a usable 1-RTT key update".into());
            }

            let mut state = client_observed.lock().unwrap();
            *state = (
                conn.local_key_phase,
                !conn.remote_key_phase,
                conn.pending_remote_key_update.is_some(),
            );
            Ok::<(), String>(())
        });

        reactor.run();

        server.unwrap_result().unwrap();
        client.unwrap_result().unwrap();
        assert_eq!(*observed.lock().unwrap(), (true, true, true));
    }

    #[test]
    fn process_retry_stores_token_and_updates_cid() {
        let mut conn = make_test_client();
        let original_dcid = conn.original_dcid.clone();
        let local_cid = conn.cid_manager.primary_local_cid().clone();

        // Verify initial state
        assert!(!conn.retry_received);
        assert!(conn.retry_token.is_empty());

        // The peer CID should be the original remote_cid
        let old_peer_cid = conn.cid_manager.active_peer_cid().clone();

        // Build a Retry with a new SCID and token
        let retry_scid = ConnectionId::from_slice(&[0x99, 0x88, 0x77, 0x66]);
        let token = b"test_retry_token_value";

        let header = packet::header::parse_header(
            &build_retry_packet(&original_dcid, &local_cid, &retry_scid, token),
            8,
        )
        .unwrap();
        let retry_pkt = build_retry_packet(&original_dcid, &local_cid, &retry_scid, token);
        let h = match &header {
            PacketHeader::Long(h) => h,
            _ => panic!("expected long header"),
        };

        conn.process_retry(&retry_pkt, h).unwrap();

        // Token should be stored
        assert_eq!(conn.retry_token, token);

        // retry_received flag should be set
        assert!(conn.retry_received);
        assert_eq!(
            conn.expected_retry_source_connection_id
                .as_ref()
                .map(ConnectionId::as_slice),
            Some(retry_scid.as_slice())
        );

        // Peer CID should be updated to the Retry SCID
        assert_eq!(
            conn.cid_manager.active_peer_cid().as_slice(),
            retry_scid.as_slice()
        );
        assert_ne!(
            conn.cid_manager.active_peer_cid().as_slice(),
            old_peer_cid.as_slice()
        );

        // Initial PN space should be reset
        assert_eq!(conn.next_pn[PacketNumberSpace::Initial as usize], 0);
        assert_eq!(conn.largest_recv_pn[PacketNumberSpace::Initial as usize], 0);
        assert!(conn.recv_pn_ranges[PacketNumberSpace::Initial as usize].is_empty());
        assert!(!conn.ack_needed[PacketNumberSpace::Initial as usize]);
        assert_eq!(
            conn.crypto_recv_offset[PacketNumberSpace::Initial as usize],
            0
        );
        // crypto_send_offset should be > 0 because the cached ClientHello was re-queued
        assert!(conn.crypto_send_offset[PacketNumberSpace::Initial as usize] > 0);
        // There should be a pending Initial CRYPTO frame with the re-queued ClientHello
        assert!(conn
            .pending_frames
            .iter()
            .any(|(s, f)| *s == PacketNumberSpace::Initial && matches!(f, Frame::Crypto(_))));
    }

    #[test]
    fn process_retry_rejects_second_retry() {
        let mut conn = make_test_client();
        let original_dcid = conn.original_dcid.clone();
        let local_cid = conn.cid_manager.primary_local_cid().clone();

        let retry_scid = ConnectionId::from_slice(&[0x99, 0x88, 0x77, 0x66]);
        let retry_pkt = build_retry_packet(&original_dcid, &local_cid, &retry_scid, b"first_token");

        let header = packet::header::parse_header(&retry_pkt, 8).unwrap();
        let h = match &header {
            PacketHeader::Long(h) => h,
            _ => panic!("expected long header"),
        };

        // First Retry should succeed
        conn.process_retry(&retry_pkt, h).unwrap();
        assert!(conn.retry_received);
        assert_eq!(conn.retry_token, b"first_token");

        // Second Retry with different token should be silently ignored
        let retry_scid2 = ConnectionId::from_slice(&[0x11, 0x22, 0x33, 0x44]);
        let retry_pkt2 =
            build_retry_packet(&original_dcid, &local_cid, &retry_scid2, b"second_token");
        let header2 = packet::header::parse_header(&retry_pkt2, 8).unwrap();
        let h2 = match &header2 {
            PacketHeader::Long(h) => h,
            _ => panic!("expected long header"),
        };

        conn.process_retry(&retry_pkt2, h2).unwrap();

        // Token should still be the first one
        assert_eq!(conn.retry_token, b"first_token");
    }

    #[test]
    fn process_retry_rejects_bad_integrity_tag() {
        let mut conn = make_test_client();
        let local_cid = conn.cid_manager.primary_local_cid().clone();

        let retry_scid = ConnectionId::from_slice(&[0x99, 0x88, 0x77, 0x66]);

        // Build retry packet with wrong original DCID (bad integrity tag)
        let wrong_dcid = ConnectionId::from_slice(&[0xff, 0xfe, 0xfd, 0xfc]);
        let retry_pkt = build_retry_packet(&wrong_dcid, &local_cid, &retry_scid, b"bad_token");

        let header = packet::header::parse_header(&retry_pkt, 8).unwrap();
        let h = match &header {
            PacketHeader::Long(h) => h,
            _ => panic!("expected long header"),
        };

        // Should return an error (integrity tag mismatch)
        let result = conn.process_retry(&retry_pkt, h);
        assert!(result.is_err());
        assert!(!conn.retry_received);
    }

    #[test]
    fn process_retry_rejects_empty_token() {
        let mut conn = make_test_client();
        let original_dcid = conn.original_dcid.clone();
        let local_cid = conn.cid_manager.primary_local_cid().clone();

        let retry_scid = ConnectionId::from_slice(&[0x99, 0x88, 0x77, 0x66]);
        let retry_pkt = build_retry_packet(&original_dcid, &local_cid, &retry_scid, b"");

        let header = packet::header::parse_header(&retry_pkt, 8).unwrap();
        let h = match &header {
            PacketHeader::Long(h) => h,
            _ => panic!("expected long header"),
        };

        let result = conn.process_retry(&retry_pkt, h);
        assert!(result.is_err());
    }

    #[test]
    fn process_retry_removes_pending_initial_frames() {
        let mut conn = make_test_client();
        let original_dcid = conn.original_dcid.clone();
        let local_cid = conn.cid_manager.primary_local_cid().clone();

        // After process_tls_output, there should be Initial CRYPTO frames pending
        let initial_frame_count = conn
            .pending_frames
            .iter()
            .filter(|(s, _)| *s == PacketNumberSpace::Initial)
            .count();
        assert!(initial_frame_count > 0, "should have Initial CRYPTO frames");

        let retry_scid = ConnectionId::from_slice(&[0x99, 0x88, 0x77, 0x66]);
        let retry_pkt = build_retry_packet(&original_dcid, &local_cid, &retry_scid, b"retry_tok");

        let header = packet::header::parse_header(&retry_pkt, 8).unwrap();
        let h = match &header {
            PacketHeader::Long(h) => h,
            _ => panic!("expected long header"),
        };

        conn.process_retry(&retry_pkt, h).unwrap();

        // Old Initial frames should be removed
        // (process_tls_output may or may not add new ones depending on rustls state)
        // Verify that no frame has crypto_send_offset from the old generation
        // The crypto_send_offset was reset to 0, so any new frames start at 0
        for (space, frame) in &conn.pending_frames {
            if *space == PacketNumberSpace::Initial {
                if let Frame::Crypto(cf) = frame {
                    assert_eq!(cf.offset, 0, "new Initial CRYPTO should start at offset 0");
                }
            }
        }
    }

    // ---- QuicStream unit tests ----

    #[test]
    fn quic_stream_send_then_recv() {
        let mut conn = make_test_client();
        let sid = conn.open_bidi_stream().unwrap();

        // Inject data into the stream's recv buffer (simulating peer sending)
        let stream = conn.streams.get_mut(sid).unwrap();
        stream.recv_buf.insert(0, b"hello world".to_vec());
        stream.recv = Some(RecvState::Recv);

        let mut buf = [0u8; 64];
        let (n, fin) = conn.stream_recv(sid, &mut buf).unwrap();
        assert_eq!(n, 11);
        assert!(!fin);
        assert_eq!(&buf[..n], b"hello world");
    }

    #[test]
    fn quic_stream_recv_fin() {
        let mut conn = make_test_client();
        let sid = conn.open_bidi_stream().unwrap();

        // Inject data with FIN
        let stream = conn.streams.get_mut(sid).unwrap();
        stream.recv_buf.insert(0, b"data".to_vec());
        stream.final_size = Some(4);
        stream.recv = Some(RecvState::SizeKnown);

        let mut buf = [0u8; 64];
        let (n, fin) = conn.stream_recv(sid, &mut buf).unwrap();
        assert_eq!(n, 4);
        assert!(fin);
        assert_eq!(&buf[..n], b"data");
        assert_eq!(
            conn.streams.get(sid).unwrap().recv,
            Some(RecvState::DataRead)
        );
    }

    #[test]
    fn quic_stream_recv_partial_reads() {
        let mut conn = make_test_client();
        let sid = conn.open_bidi_stream().unwrap();

        let stream = conn.streams.get_mut(sid).unwrap();
        stream.recv_buf.insert(0, b"abcdef".to_vec());
        stream.recv = Some(RecvState::Recv);

        // Read with a small buffer
        let mut buf = [0u8; 3];
        let (n, fin) = conn.stream_recv(sid, &mut buf).unwrap();
        assert_eq!(n, 3);
        assert!(!fin);
        assert_eq!(&buf[..n], b"abc");

        // Read the rest
        let (n, _) = conn.stream_recv(sid, &mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf[..n], b"def");
    }

    #[test]
    fn quic_stream_close_queues_fin() {
        let mut conn = make_connected_test_client();
        let sid = conn.open_bidi_stream().unwrap();

        // Sending with fin=true queues a FIN
        let n = conn.stream_send(sid, &[], true).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn stream_send_short_write_suppresses_fin() {
        let mut conn = make_connected_test_client();
        let sid = conn.open_bidi_stream().unwrap();

        // Shrink the stream send window so the write is short
        let stream = conn.streams.get_mut(sid).unwrap();
        stream.flow_control = crate::transport::flow_control::StreamFlowControl::new(10, 10);

        // Try to send 20 bytes with fin=true — only 10 fit
        let data = vec![0xAB; 20];
        let n = conn.stream_send(sid, &data, true).unwrap();
        assert_eq!(n, 10, "should short-write to the window size");

        // The queued frame must NOT carry FIN
        let (_, frame) = conn.pending_frames.back().unwrap();
        match frame {
            Frame::Stream(sf) => {
                assert!(!sf.fin, "FIN must not be sent on a short write");
                assert_eq!(sf.data.len(), 10);
            }
            other => panic!("expected Stream frame, got {:?}", other),
        }

        // Stream should NOT be in DataSent state
        let stream = conn.streams.get(sid).unwrap();
        assert!(!stream.fin_sent);
        assert_ne!(stream.send, Some(SendState::DataSent));
    }

    // ---- QuicConnection core API tests ----

    #[test]
    fn open_uni_stream_returns_valid_id() {
        let mut conn = make_test_client();
        let stream_id = conn.open_uni_stream().unwrap();
        // Client-initiated unidirectional streams: 2, 6, 10, ... (RFC 9000 §2.1)
        assert_eq!(stream_id, 2);
        let stream_id2 = conn.open_uni_stream().unwrap();
        assert_eq!(stream_id2, 6);
    }

    #[test]
    fn accept_incoming_none_when_empty() {
        let conn = make_test_client();
        assert!(conn.accept_incoming_bidi().is_none());
        assert!(conn.accept_incoming_uni().is_none());
    }

    #[test]
    fn feed_datagram_rejects_truncated() {
        let mut conn = make_test_client();
        // A single byte is too short to parse any valid QUIC header
        let result = conn.feed_datagram(&[0xff]);
        assert!(result.is_err());
    }

    #[test]
    fn feed_datagram_empty_is_noop() {
        let mut conn = make_test_client();
        // Empty datagram is silently ignored (no bytes to parse)
        let result = conn.feed_datagram(&[]);
        assert!(result.is_ok());
    }

    // ---- Stateless reset tests (QUIC-23) ----

    #[test]
    fn stateless_reset_detected_and_drains() {
        let mut conn = make_test_client();
        let token: [u8; 16] = [0xAA; 16];

        // Register a peer CID with a known stateless reset token
        let peer_cid = crate::packet::ConnectionId::from_slice(&[9, 10, 11, 12]);
        conn.cid_manager
            .add_peer_cid(1, peer_cid, token, 0)
            .unwrap();

        // Build a fake datagram: short header form (bit 7 = 0) + random bytes + token
        let mut datagram = vec![0x40u8]; // short header form, fixed bit set
        datagram.extend_from_slice(&[0u8; 20]); // random padding
        datagram.extend_from_slice(&token); // last 16 bytes = reset token

        // feed_datagram should detect the stateless reset and transition to Draining
        let result = conn.feed_datagram(&datagram);
        assert!(result.is_ok());
        assert_eq!(conn.state(), ConnectionState::Draining);
    }

    #[test]
    fn stateless_reset_not_matched_propagates_error() {
        let mut conn = make_test_client();

        // No peer tokens registered (initial CID has no token).
        // Build a fake datagram that looks like a short header but can't decrypt.
        let mut datagram = vec![0x40u8]; // short header form
        datagram.extend_from_slice(&[0u8; 30]); // padding + fake token

        let result = conn.feed_datagram(&datagram);
        // Without matching token, should NOT drain. The packet is silently
        // dropped (no 1-RTT keys yet), which is per RFC 9000 §5.2.2.
        assert!(result.is_ok());
        assert_ne!(conn.state(), ConnectionState::Draining);
    }

    #[test]
    fn undecryptable_protected_packet_is_silently_dropped() {
        let mut conn = make_test_client();
        let local_cid = conn.cid_manager.primary_local_cid().clone();
        let peer_cid = conn.cid_manager.active_peer_cid().clone();
        let (mut datagram, _, _) = crate::packet::initial::encode_initial_packet(
            &local_cid,
            &peer_cid,
            &[],
            0,
            0,
            &[0u8; 32],
            16,
        )
        .unwrap();
        datagram.extend_from_slice(&[0u8; 16]);

        let result = conn.feed_datagram(&datagram);
        assert!(result.is_ok(), "{result:?}");
        assert_ne!(conn.state(), ConnectionState::Draining);
    }

    #[test]
    fn client_updates_peer_cid_from_first_server_packet() {
        let mut conn = make_test_client();
        let local_cid = conn.cid_manager.primary_local_cid().clone();
        let original_peer_cid = conn.cid_manager.active_peer_cid().clone();
        let server_cid = ConnectionId::from_slice(&[0x90, 0x91, 0x92, 0x93]);

        let (mut datagram, _, _) = crate::packet::initial::encode_initial_packet(
            &local_cid,
            &server_cid,
            &[],
            0,
            0,
            &[0u8; 32],
            16,
        )
        .unwrap();
        datagram.extend_from_slice(&[0u8; 16]);

        let _ = conn.feed_datagram(&datagram);

        assert_eq!(
            conn.peer_initial_source_connection_id
                .as_ref()
                .map(ConnectionId::as_slice),
            Some(server_cid.as_slice())
        );
        assert_eq!(
            conn.cid_manager.active_peer_cid().as_slice(),
            server_cid.as_slice()
        );
        assert_ne!(
            conn.cid_manager.active_peer_cid().as_slice(),
            original_peer_cid.as_slice()
        );
    }

    #[test]
    fn version_negotiation_without_v1_is_rejected() {
        let mut conn = make_test_client();
        let packet = crate::packet::version_negotiation::encode_version_negotiation(
            conn.cid_manager.primary_local_cid(),
            conn.cid_manager.active_peer_cid(),
            &[0x0a0a_0a0a],
        );

        let err = conn.feed_datagram(&packet).unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::InternalError, _)
        ));
    }

    #[test]
    fn stateless_reset_too_short_ignored() {
        let mut conn = make_test_client();
        let token: [u8; 16] = [0xBB; 16];
        let peer_cid = crate::packet::ConnectionId::from_slice(&[1, 2, 3, 4]);
        conn.cid_manager
            .add_peer_cid(1, peer_cid, token, 0)
            .unwrap();

        // Datagram shorter than 21 bytes — can't be a valid stateless reset.
        // With only 17 bytes it's also too short for a valid short header
        // (1 byte header + 8 byte DCID = 9 minimum), but parse_header
        // accepts it. Without 1-RTT keys the packet is silently dropped.
        let mut datagram = vec![0x40u8];
        datagram.extend_from_slice(&token); // 17 bytes total < 21
        let result = conn.feed_datagram(&datagram);
        assert!(result.is_ok());
        assert_ne!(conn.state(), ConnectionState::Draining);
    }

    #[test]
    fn process_frame_connection_close_sets_draining() {
        let mut conn = make_test_client();
        let frame = Frame::ConnectionClose {
            is_transport: true,
            error_code: 0x00,
            frame_type: Some(0),
            reason: b"bye".to_vec(),
        };
        conn.process_frame(PacketNumberSpace::Initial, frame)
            .unwrap();
        assert_eq!(conn.state(), ConnectionState::Draining);
    }
    #[test]
    fn process_frame_max_streams_rejects_value_above_limit() {
        let mut conn = make_test_client();
        let err = conn
            .process_frame(
                PacketNumberSpace::ApplicationData,
                Frame::MaxStreams {
                    bidirectional: true,
                    maximum_streams: MAX_STREAMS_LIMIT + 1,
                },
            )
            .unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::FrameEncodingError, _)
        ));
    }

    #[test]
    fn process_frame_new_connection_id_rejects_invalid_retire_prior_to() {
        let mut conn = make_test_client();
        let err = conn
            .process_frame(
                PacketNumberSpace::ApplicationData,
                Frame::NewConnectionId {
                    sequence_number: 1,
                    retire_prior_to: 2,
                    connection_id: vec![0xaa, 0xbb, 0xcc, 0xdd],
                    stateless_reset_token: [0u8; 16],
                },
            )
            .unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::FrameEncodingError, _)
        ));
    }

    #[test]
    fn process_frame_new_connection_id_rejects_empty_cid() {
        let mut conn = make_test_client();
        let err = conn
            .process_frame(
                PacketNumberSpace::ApplicationData,
                Frame::NewConnectionId {
                    sequence_number: 1,
                    retire_prior_to: 0,
                    connection_id: vec![],
                    stateless_reset_token: [0u8; 16],
                },
            )
            .unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::FrameEncodingError, _)
        ));
    }

    #[test]
    fn process_frame_stop_sending_unknown_stream_errors() {
        let mut conn = make_test_client();
        let err = conn
            .process_frame(
                PacketNumberSpace::ApplicationData,
                Frame::StopSending {
                    stream_id: 0,
                    application_error_code: 7,
                },
            )
            .unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::StreamStateError, _)
        ));
    }

    #[test]
    fn process_frame_stop_sending_accepts_unknown_peer_initiated_bidi_stream() {
        let mut conn = make_test_client();
        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::StopSending {
                stream_id: 1,
                application_error_code: 7,
            },
        )
        .unwrap();

        let stream = conn.streams.get(1).unwrap();
        assert_eq!(stream.send, Some(SendState::ResetSent));
        assert!(matches!(
            conn.pending_frames.back(),
            Some((
                PacketNumberSpace::ApplicationData,
                Frame::ResetStream {
                    stream_id: 1,
                    application_error_code: 7,
                    final_size: 0,
                },
            ))
        ));
    }

    #[test]
    fn process_frame_new_connection_id_conflicting_duplicate_is_protocol_violation() {
        let mut conn = make_test_client();
        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::NewConnectionId {
                sequence_number: 1,
                retire_prior_to: 0,
                connection_id: vec![0xaa, 0xbb, 0xcc, 0xdd],
                stateless_reset_token: [0x11; 16],
            },
        )
        .unwrap();

        let err = conn
            .process_frame(
                PacketNumberSpace::ApplicationData,
                Frame::NewConnectionId {
                    sequence_number: 1,
                    retire_prior_to: 0,
                    connection_id: vec![0xde, 0xad, 0xbe, 0xef],
                    stateless_reset_token: [0x22; 16],
                },
            )
            .unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::ProtocolViolation, _)
        ));
    }

    #[test]
    fn process_frame_stream_transitions_to_data_recvd_when_final_range_complete() {
        let mut conn = make_test_client();
        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::Stream(StreamFrame {
                stream_id: 1,
                offset: 0,
                fin: true,
                data: b"data".to_vec(),
            }),
        )
        .unwrap();

        let stream = conn.streams.get(1).unwrap();
        assert_eq!(stream.recv, Some(RecvState::DataRecvd));
        assert_eq!(stream.final_size, Some(4));

        let mut buf = [0u8; 8];
        let (n, fin) = conn.stream_recv(1, &mut buf).unwrap();
        assert_eq!(n, 4);
        assert!(fin);
        assert_eq!(&buf[..n], b"data");
        assert_eq!(conn.streams.get(1).unwrap().recv, Some(RecvState::DataRead));
    }

    #[test]
    fn process_frame_max_data_updates_flow_control() {
        let mut conn = make_test_client();
        let old_window = conn.flow_control.send_window();
        let frame = Frame::MaxData {
            maximum_data: 999_999,
        };
        conn.process_frame(PacketNumberSpace::ApplicationData, frame)
            .unwrap();
        // Send window should increase after MAX_DATA
        assert!(conn.flow_control.send_window() >= old_window);
    }

    #[test]
    fn process_frame_ping_is_noop() {
        let mut conn = make_test_client();
        let result = conn.process_frame(PacketNumberSpace::ApplicationData, Frame::Ping);
        assert!(result.is_ok());
    }

    #[test]
    fn process_frame_padding_is_noop() {
        let mut conn = make_test_client();
        let result = conn.process_frame(PacketNumberSpace::ApplicationData, Frame::Padding);
        assert!(result.is_ok());
    }

    // ---- process_frame tests for ResetStream, Stream data, NewConnectionId, HandshakeDone ----

    #[test]
    fn process_frame_reset_stream() {
        let mut conn = make_test_client();
        // Accept a remote-initiated bidi stream first
        let sid = 1; // server-initiated bidi
        conn.streams.accept_stream(sid).unwrap();

        let frame = Frame::ResetStream {
            stream_id: sid,
            application_error_code: 42,
            final_size: 100,
        };
        conn.process_frame(PacketNumberSpace::ApplicationData, frame)
            .unwrap();

        let stream = conn.streams.get(sid).unwrap();
        assert_eq!(stream.final_size, Some(100));
        assert_eq!(stream.reset_error_code, Some(42));
        assert_eq!(stream.recv, Some(RecvState::ResetRecvd));
        assert!(stream.recv_buf.is_empty());
    }

    #[test]
    fn stream_recv_reports_reset_stream_error_code() {
        let mut conn = make_test_client();
        let sid = 1;
        conn.streams.accept_stream(sid).unwrap();
        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::ResetStream {
                stream_id: sid,
                application_error_code: 42,
                final_size: 100,
            },
        )
        .unwrap();

        let err = conn.stream_recv(sid, &mut [0u8; 8]).unwrap_err();
        assert!(matches!(err, QuicError::StreamReset(42)));
    }

    #[test]
    fn process_frame_stream_data_ingestion() {
        let mut conn = make_test_client();
        let sid = 1; // remote-initiated bidi

        let frame = Frame::Stream(crate::frame::stream::StreamFrame {
            stream_id: sid,
            offset: 0,
            fin: false,
            data: b"hello".to_vec(),
        });
        conn.process_frame(PacketNumberSpace::ApplicationData, frame)
            .unwrap();

        // Stream should be auto-accepted and data buffered
        let stream = conn.streams.get(sid).unwrap();
        assert_eq!(stream.recv_buf.get(&0).unwrap(), b"hello");
    }

    #[test]
    fn process_frame_stream_data_with_fin() {
        let mut conn = make_test_client();
        let sid = 1;

        let frame = Frame::Stream(crate::frame::stream::StreamFrame {
            stream_id: sid,
            offset: 0,
            fin: true,
            data: b"done".to_vec(),
        });
        conn.process_frame(PacketNumberSpace::ApplicationData, frame)
            .unwrap();

        let stream = conn.streams.get(sid).unwrap();
        assert_eq!(stream.final_size, Some(4));
        assert_eq!(stream.recv, Some(RecvState::DataRecvd));
    }

    #[test]
    fn process_frame_stream_data_merges_overlapping_segments() {
        let mut conn = make_test_client();
        let sid = 1;

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::Stream(crate::frame::stream::StreamFrame {
                stream_id: sid,
                offset: 0,
                fin: false,
                data: b"hello".to_vec(),
            }),
        )
        .unwrap();
        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::Stream(crate::frame::stream::StreamFrame {
                stream_id: sid,
                offset: 2,
                fin: false,
                data: b"llo world".to_vec(),
            }),
        )
        .unwrap();

        let mut buf = [0u8; 32];
        let (n, fin) = conn.stream_recv(sid, &mut buf).unwrap();
        assert_eq!(n, 11);
        assert!(!fin);
        assert_eq!(&buf[..n], b"hello world");
    }

    #[test]
    fn stream_recv_preserves_partial_remainder_on_same_offset_retransmit() {
        let mut conn = make_test_client();
        let sid = conn.open_bidi_stream().unwrap();

        let stream = conn.streams.get_mut(sid).unwrap();
        stream.recv_buf.insert(0, b"abcdefghi".to_vec());
        stream.recv = Some(RecvState::Recv);

        let mut buf = [0u8; 3];
        let (n, fin) = conn.stream_recv(sid, &mut buf).unwrap();
        assert_eq!(n, 3);
        assert!(!fin);
        assert_eq!(&buf[..n], b"abc");

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::Stream(crate::frame::stream::StreamFrame {
                stream_id: sid,
                offset: 3,
                fin: false,
                data: b"def".to_vec(),
            }),
        )
        .unwrap();

        let mut tail = [0u8; 16];
        let (n, fin) = conn.stream_recv(sid, &mut tail).unwrap();
        assert_eq!(n, 6);
        assert!(!fin);
        assert_eq!(&tail[..n], b"defghi");
    }

    #[test]
    fn process_frame_stream_data_rejects_send_only_stream() {
        let mut conn = make_test_client();
        let sid = conn.open_uni_stream().unwrap(); // local uni is send-only

        let frame = Frame::Stream(crate::frame::stream::StreamFrame {
            stream_id: sid,
            offset: 0,
            fin: false,
            data: b"invalid".to_vec(),
        });

        let err = conn
            .process_frame(PacketNumberSpace::ApplicationData, frame)
            .unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::StreamStateError, _)
        ));
    }

    #[test]
    fn process_frame_reset_stream_rejects_send_only_stream() {
        let mut conn = make_test_client();
        let sid = conn.open_uni_stream().unwrap(); // local uni is send-only

        let err = conn
            .process_frame(
                PacketNumberSpace::ApplicationData,
                Frame::ResetStream {
                    stream_id: sid,
                    application_error_code: 42,
                    final_size: 0,
                },
            )
            .unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::StreamStateError, _)
        ));
    }

    #[test]
    fn process_frame_stream_data_creates_implicit_lower_remote_streams() {
        let mut conn = make_test_client();
        let sid = 9; // server-initiated bidi, implicitly opens 1 and 5 too

        let frame = Frame::Stream(crate::frame::stream::StreamFrame {
            stream_id: sid,
            offset: 0,
            fin: false,
            data: b"hello".to_vec(),
        });
        conn.process_frame(PacketNumberSpace::ApplicationData, frame)
            .unwrap();

        assert!(conn.streams.get(1).is_some());
        assert!(conn.streams.get(5).is_some());
        assert!(conn.streams.get(9).is_some());
    }

    #[test]
    fn process_frame_stream_data_enforces_remote_stream_limit() {
        let mut conn = make_test_client();
        conn.streams = StreamManager::new(Side::Client, 1, 100, DEFAULT_INITIAL_MAX_STREAM_DATA);

        let frame = Frame::Stream(crate::frame::stream::StreamFrame {
            stream_id: 5, // second server-initiated bidi stream
            offset: 0,
            fin: false,
            data: b"hello".to_vec(),
        });

        let err = conn
            .process_frame(PacketNumberSpace::ApplicationData, frame)
            .unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::StreamLimitError, _)
        ));
    }

    #[test]
    fn process_frame_new_connection_id() {
        let mut conn = make_test_client();
        let frame = Frame::NewConnectionId {
            sequence_number: 1,
            retire_prior_to: 0,
            connection_id: vec![0xaa, 0xbb, 0xcc, 0xdd],
            stateless_reset_token: [0u8; 16],
        };
        conn.process_frame(PacketNumberSpace::ApplicationData, frame)
            .unwrap();
        // Peer CID should still be the original (seq 0), but the new one is stored
    }

    #[test]
    fn process_frame_new_token_client_caches_latest_token() {
        let mut conn = make_test_client();
        let token_cache: ClientTokenCache = Arc::new(Mutex::new(HashMap::new()));
        let cache_key = "localhost:h3".to_string();
        conn.peer_transport_parameter_cache_key = Some(cache_key.clone());
        conn.client_token_cache = Some(token_cache.clone());
        conn.retry_token = vec![0x01, 0x02];

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::NewToken {
                token: vec![0xaa, 0xbb, 0xcc],
            },
        )
        .unwrap();

        assert_eq!(conn.retry_token, vec![0xaa, 0xbb, 0xcc]);
        assert_eq!(
            token_cache.lock().unwrap().get(&cache_key).cloned(),
            Some(vec![0xaa, 0xbb, 0xcc])
        );
    }

    #[test]
    fn process_frame_new_token_server_is_protocol_violation() {
        let mut conn = make_test_client();
        conn.side = Side::Server;

        let err = conn
            .process_frame(
                PacketNumberSpace::ApplicationData,
                Frame::NewToken {
                    token: vec![0xaa, 0xbb, 0xcc],
                },
            )
            .unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::ProtocolViolation, _)
        ));
    }

    #[test]
    fn queue_new_token_frame_server_queues_token_once() {
        let mut conn = make_test_client();
        enable_application_data(&mut conn);
        conn.side = Side::Server;
        conn.new_token_secret = Some([0x33; 32]);

        conn.queue_new_token_frame();
        conn.queue_new_token_frame();

        let new_tokens: Vec<Vec<u8>> = conn
            .pending_frames
            .iter()
            .filter_map(|(space, frame)| match (space, frame) {
                (&PacketNumberSpace::ApplicationData, Frame::NewToken { token }) => {
                    Some(token.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(new_tokens.len(), 1);
        assert!(retry::validate_new_token(&new_tokens[0], &conn.peer_addr, &[0x33; 32]).unwrap());
    }

    #[test]
    fn retire_connection_id_for_active_local_cid_issues_replacement() {
        let mut conn = make_connected_test_client();
        conn.peer_active_connection_id_limit = 2;

        conn.generate_connection_id_updates();
        conn.pending_frames.clear();

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::RetireConnectionId { sequence_number: 0 },
        )
        .unwrap();

        conn.generate_connection_id_updates();

        assert!(conn.pending_frames.iter().any(|(_, frame)| matches!(
            frame,
            Frame::NewConnectionId {
                sequence_number,
                retire_prior_to: 1,
                ..
            } if *sequence_number > 0
        )));
    }

    #[test]
    fn process_frame_handshake_done_client() {
        let mut conn = make_test_client();
        // Set state to Handshake first (as would be the case in real flow)
        conn.state = ConnectionState::Handshake;

        conn.process_frame(PacketNumberSpace::ApplicationData, Frame::HandshakeDone)
            .unwrap();
        assert_eq!(conn.state(), ConnectionState::Connected);
    }

    #[test]
    fn process_frame_stream_flow_control_violation() {
        let mut conn = make_test_client();
        let sid = 1;

        // Send data exceeding the per-stream recv limit.
        // The stream flow control recv_max is DEFAULT_INITIAL_MAX_STREAM_DATA (262144).
        let frame = Frame::Stream(crate::frame::stream::StreamFrame {
            stream_id: sid,
            offset: 0,
            fin: false,
            data: vec![0u8; (DEFAULT_INITIAL_MAX_STREAM_DATA + 1) as usize],
        });
        let result = conn.process_frame(PacketNumberSpace::ApplicationData, frame);
        assert!(result.is_err());
    }

    #[test]
    fn process_frame_connection_flow_control_violation() {
        let mut conn = make_test_client();

        // Per-stream limit is 256 KB, connection limit is 1 MB.
        // Send data across 5 streams, each at the stream limit (256 KB).
        // Total = 5 * 256 KB = 1.25 MB > 1 MB connection limit.
        let per_stream = DEFAULT_INITIAL_MAX_STREAM_DATA as usize; // 262144

        for i in 0..5u64 {
            let sid = 1 + i * 4; // server-initiated bidi: 1, 5, 9, 13, 17
            let frame = Frame::Stream(crate::frame::stream::StreamFrame {
                stream_id: sid,
                offset: 0,
                fin: false,
                data: vec![0u8; per_stream],
            });
            let result = conn.process_frame(PacketNumberSpace::ApplicationData, frame);
            if result.is_err() {
                // Should fail on the stream that pushes total past 1 MB
                assert!(
                    i >= 4,
                    "connection flow control should trigger after 4 full streams, failed at stream {i}"
                );
                return;
            }
        }
        panic!("expected connection-level FlowControlError");
    }

    #[test]
    fn stream_send_respects_connection_level_flow_control_and_unblocks_after_max_data() {
        let mut conn = make_connected_test_client();
        conn.flow_control.set_send_max(8);

        let first = conn.open_bidi_stream().unwrap();
        let second = conn.open_bidi_stream().unwrap();

        assert_eq!(conn.stream_send(first, b"hello", false).unwrap(), 5);
        assert_eq!(conn.stream_send(second, b"world", false).unwrap(), 3);
        assert_eq!(conn.flow_control.send_window(), 0);

        conn.process_frame(
            PacketNumberSpace::ApplicationData,
            Frame::MaxData { maximum_data: 12 },
        )
        .unwrap();

        assert_eq!(conn.stream_send(second, b"ld", false).unwrap(), 2);
    }

    // ---- find_incoming tests ----

    #[test]
    fn generate_flow_control_updates_queues_max_data() {
        let mut conn = make_test_client();

        // Connection window = 1 MB, half = 512 KB.
        // Per-stream limit = 256 KB, so spread across 3 streams (200 KB each = 600 KB total).
        let per_stream = 200_000usize;
        for i in 0..3u64 {
            let sid = 1 + i * 4; // server-initiated bidi: 1, 5, 9
            let frame = Frame::Stream(crate::frame::stream::StreamFrame {
                stream_id: sid,
                offset: 0,
                fin: false,
                data: vec![0u8; per_stream],
            });
            conn.process_frame(PacketNumberSpace::ApplicationData, frame)
                .unwrap();
        }

        // generate_flow_control_updates should queue a MAX_DATA frame.
        conn.generate_flow_control_updates();

        let has_max_data = conn
            .pending_frames
            .iter()
            .any(|(_, f)| matches!(f, Frame::MaxData { .. }));
        assert!(has_max_data, "expected MAX_DATA frame in pending_frames");
    }

    #[test]
    fn generate_flow_control_updates_queues_max_stream_data() {
        let mut conn = make_test_client();

        // Receive data past the half-window threshold on a single stream.
        // Stream window = 256 KB, threshold = 256 KB - 128 KB = 128 KB.
        let sid = 1;
        let half_plus_one = (DEFAULT_INITIAL_MAX_STREAM_DATA / 2 + 1) as usize;
        let frame = Frame::Stream(crate::frame::stream::StreamFrame {
            stream_id: sid,
            offset: 0,
            fin: false,
            data: vec![0u8; half_plus_one],
        });
        conn.process_frame(PacketNumberSpace::ApplicationData, frame)
            .unwrap();

        conn.generate_flow_control_updates();

        let has_max_stream_data = conn
            .pending_frames
            .iter()
            .any(|(_, f)| matches!(f, Frame::MaxStreamData { stream_id, .. } if *stream_id == sid));
        assert!(
            has_max_stream_data,
            "expected MAX_STREAM_DATA frame in pending_frames"
        );
    }

    #[test]
    fn generate_flow_control_updates_skips_send_only_streams() {
        let mut conn = make_test_client();
        let sid = conn.open_uni_stream().unwrap();

        conn.generate_flow_control_updates();

        let has_max_stream_data = conn
            .pending_frames
            .iter()
            .any(|(_, f)| matches!(f, Frame::MaxStreamData { stream_id, .. } if *stream_id == sid));
        assert!(
            !has_max_stream_data,
            "did not expect MAX_STREAM_DATA for send-only stream"
        );
    }

    // ---- find_incoming tests ----

    #[test]
    fn find_incoming_bidi_with_data() {
        let mut conn = make_test_client();
        // Inject a server-initiated bidi stream with data
        let sid = 1; // server-initiated bidi (bit 0 = 1, bit 1 = 0)
        conn.streams.accept_stream(sid).unwrap();
        conn.streams
            .get_mut(sid)
            .unwrap()
            .recv_buf
            .insert(0, b"request".to_vec());

        assert_eq!(conn.accept_incoming_bidi(), Some(sid));
    }

    #[test]
    fn find_incoming_bidi_without_data() {
        let mut conn = make_test_client();
        let sid = 1;
        conn.streams.accept_stream(sid).unwrap();
        // No data in recv_buf
        assert!(conn.accept_incoming_bidi().is_none());
    }

    #[test]
    fn find_incoming_uni_with_data() {
        let mut conn = make_test_client();
        // Server-initiated unidirectional stream (bit 0 = 1, bit 1 = 1)
        let sid = 3;
        conn.streams.accept_stream(sid).unwrap();
        conn.streams
            .get_mut(sid)
            .unwrap()
            .recv_buf
            .insert(0, b"control".to_vec());

        assert_eq!(conn.accept_incoming_uni(), Some(sid));
    }

    #[test]
    fn find_incoming_uni_without_data() {
        let mut conn = make_test_client();
        let sid = 3;
        conn.streams.accept_stream(sid).unwrap();
        assert!(conn.accept_incoming_uni().is_none());
    }

    // ---- split_frame tests ----

    #[test]
    fn split_frame_crypto_splits_at_budget() {
        // CRYPTO frame with 2000 bytes of data — must be split for a small budget.
        let frame = Frame::Crypto(crate::frame::crypto::CryptoFrame {
            offset: 100,
            data: vec![0xAA; 2000],
        });

        // Budget of 500 bytes: overhead is ~5 bytes (type + offset + length varints),
        // so roughly 495 bytes of data should fit.
        let (head, tail) = QuicConnection::split_frame(frame, 500).unwrap();

        match (&head, &tail) {
            (Frame::Crypto(h), Frame::Crypto(t)) => {
                // Head must fit within budget
                let mut buf = Vec::new();
                head.encode(&mut buf).unwrap();
                assert!(buf.len() <= 500, "head {} > budget 500", buf.len());
                // Head starts at original offset
                assert_eq!(h.offset, 100);
                // Tail starts where head ended
                assert_eq!(t.offset, 100 + h.data.len() as u64);
                // All data accounted for
                assert_eq!(h.data.len() + t.data.len(), 2000);
            }
            _ => panic!("expected CRYPTO frames"),
        }
    }

    #[test]
    fn split_frame_stream_preserves_fin_on_tail() {
        let frame = Frame::Stream(crate::frame::stream::StreamFrame {
            stream_id: 4,
            offset: 0,
            fin: true,
            data: vec![0xBB; 2000],
        });

        let (head, tail) = QuicConnection::split_frame(frame, 500).unwrap();

        match (&head, &tail) {
            (Frame::Stream(h), Frame::Stream(t)) => {
                assert!(!h.fin, "FIN must not be on the head fragment");
                assert!(t.fin, "FIN must be on the tail fragment");
                assert_eq!(h.stream_id, 4);
                assert_eq!(t.stream_id, 4);
                assert_eq!(t.offset, h.data.len() as u64);
                assert_eq!(h.data.len() + t.data.len(), 2000);
            }
            _ => panic!("expected STREAM frames"),
        }
    }

    #[test]
    fn split_frame_uses_actual_fragment_encoding_budget() {
        let frame = Frame::Crypto(crate::frame::crypto::CryptoFrame {
            offset: 0,
            data: vec![0xAA; 200],
        });
        let mut expected_head = Vec::new();
        Frame::Crypto(crate::frame::crypto::CryptoFrame {
            offset: 0,
            data: vec![0xAA; 63],
        })
        .encode(&mut expected_head)
        .unwrap();

        let (head, tail) = QuicConnection::split_frame(frame, expected_head.len()).unwrap();

        match (&head, &tail) {
            (Frame::Crypto(h), Frame::Crypto(t)) => {
                let mut head_buf = Vec::new();
                head.encode(&mut head_buf).unwrap();
                assert_eq!(head_buf.len(), expected_head.len());
                assert_eq!(h.data.len(), 63);
                assert_eq!(h.data.len() + t.data.len(), 200);
            }
            _ => panic!("expected CRYPTO frames"),
        }
    }

    #[test]
    fn split_frame_returns_none_for_small_frame() {
        let frame = Frame::Crypto(crate::frame::crypto::CryptoFrame {
            offset: 0,
            data: vec![0xCC; 10],
        });
        // Budget larger than the encoded frame — no split needed
        assert!(QuicConnection::split_frame(frame, 500).is_none());
    }

    #[test]
    fn split_frame_returns_none_for_non_data_frame() {
        assert!(QuicConnection::split_frame(Frame::Ping, 500).is_none());
        assert!(QuicConnection::split_frame(Frame::Padding, 500).is_none());
        assert!(QuicConnection::split_frame(
            Frame::MaxData {
                maximum_data: 99999
            },
            5
        )
        .is_none());
    }

    #[test]
    fn record_recv_pn_merges_ranges_across_out_of_order_insertions() {
        let mut conn = make_connected_test_client();

        for pn in [10, 8, 9, 6, 7, 5, 3, 4] {
            conn.record_recv_pn(PacketNumberSpace::ApplicationData, pn);
        }

        assert_eq!(
            conn.recv_pn_ranges[PacketNumberSpace::ApplicationData as usize],
            vec![(3, 10)]
        );
    }

    #[test]
    fn record_recv_pn_trims_oldest_ranges() {
        let mut conn = make_connected_test_client();

        for pn in (0..(MAX_TRACKED_RECV_PN_RANGES as u64 + 4)).map(|n| n * 2) {
            conn.record_recv_pn(PacketNumberSpace::ApplicationData, pn);
        }

        let ranges = &conn.recv_pn_ranges[PacketNumberSpace::ApplicationData as usize];
        assert_eq!(ranges.len(), MAX_TRACKED_RECV_PN_RANGES);
        assert_eq!(ranges.first(), Some(&(70, 70)));
        assert_eq!(ranges.last(), Some(&(8, 8)));
    }

    #[test]
    fn packet_number_adjacency_does_not_wrap_at_u64_max() {
        assert!(QuicConnection::packet_numbers_are_adjacent(
            u64::MAX - 1,
            u64::MAX
        ));
        assert!(!QuicConnection::packet_numbers_are_adjacent(u64::MAX, 0));
        assert!(!QuicConnection::packet_numbers_are_adjacent(
            u64::MAX,
            u64::MAX
        ));
    }
}
