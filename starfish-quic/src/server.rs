//! QUIC server — accepts incoming QUIC connections.
//!
//! [`QuicListener`] binds a UDP socket and accepts individual connections.
//! [`QuicEndpoint`] provides DCID-based datagram routing for concurrent
//! connections on a single socket. Both support optional address validation
//! via Retry packets ([`QuicServerConfig::require_retry`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use rustls::ServerConfig;
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;
use starfish_reactor::cooperative_io::udp_socket::{UdpEcnCodepoint, UdpSocket};

use crate::connection::{
    QuicConnection, DEFAULT_IDLE_TIMEOUT_MS, DEFAULT_INITIAL_MAX_DATA,
    DEFAULT_INITIAL_MAX_STREAMS_BIDI, DEFAULT_INITIAL_MAX_STREAMS_UNI,
    DEFAULT_INITIAL_MAX_STREAM_DATA, MAX_LOCAL_UDP_PAYLOAD_SIZE,
};
use crate::error::QuicError;
use crate::packet::header::{self, LongPacketType, PacketHeader};
use crate::packet::retry;
use crate::packet::{self, ConnectionId};
use crate::transport::stream_manager::StreamId;

/// Configuration for a QUIC server.
pub struct QuicServerConfig {
    /// TLS server configuration.
    pub tls_config: ServerConfig,
    /// Connection ID length (default: 8 bytes).
    pub cid_len: u8,
    /// Whether to require address validation via Retry packets (RFC 9000 §8.1.2).
    ///
    /// When enabled, the server sends a Retry packet in response to the first
    /// Initial from a client. The client must echo the token in a new Initial.
    pub require_retry: bool,
    /// Secret key for generating and validating Retry tokens.
    /// Randomly generated at construction time.
    pub(crate) retry_secret: [u8; 32],
}

impl QuicServerConfig {
    /// Create a new QuicServerConfig with the given TLS config.
    pub fn new(tls_config: ServerConfig) -> Self {
        use rand::Rng;
        let mut retry_secret = [0u8; 32];
        rand::rng().fill(&mut retry_secret);
        Self {
            tls_config,
            cid_len: 8,
            require_retry: false,
            retry_secret,
        }
    }

    /// Enable Retry-based address validation.
    pub fn with_retry(mut self) -> Self {
        self.require_retry = true;
        self
    }

    /// Enable QUIC 0-RTT acceptance for resumed sessions.
    pub fn enable_early_data(mut self) -> Self {
        self.tls_config.max_early_data_size = u32::MAX;
        self
    }
}

/// Maximum UDP datagram size for receiving.
const MAX_RECV_SIZE: usize = MAX_LOCAL_UDP_PAYLOAD_SIZE;

fn generate_stateless_reset_token() -> [u8; 16] {
    use rand::Rng;

    let mut token = [0u8; 16];
    rand::rng().fill(&mut token);
    token
}

fn queue_established_handle(
    queue: &mut VecDeque<ConnectionHandle>,
    queued: &mut HashSet<ConnectionHandle>,
    handle: ConnectionHandle,
) {
    if queued.insert(handle) {
        queue.push_back(handle);
    }
}

fn dequeue_established_handle(
    queue: &mut VecDeque<ConnectionHandle>,
    queued: &mut HashSet<ConnectionHandle>,
) -> Option<ConnectionHandle> {
    let handle = queue.pop_front()?;
    queued.remove(&handle);
    Some(handle)
}

fn unqueue_established_handle(
    queue: &mut VecDeque<ConnectionHandle>,
    queued: &mut HashSet<ConnectionHandle>,
    handle: ConnectionHandle,
) {
    queued.remove(&handle);
    queue.retain(|queued_handle| *queued_handle != handle);
}

fn register_connection_cids(
    cid_to_handle: &mut HashMap<Vec<u8>, ConnectionHandle>,
    handle: ConnectionHandle,
    conn: &QuicConnection,
) {
    for cid in conn.local_cid_bytes() {
        cid_to_handle.insert(cid, handle);
    }
}

fn register_connection_reset_tokens(
    cid_to_reset_token: &mut HashMap<Vec<u8>, [u8; 16]>,
    conn: &QuicConnection,
) {
    for (cid, token) in conn.local_cid_reset_tokens() {
        cid_to_reset_token.insert(cid, token);
    }
}

fn cids_for_handle(
    cid_to_handle: &HashMap<Vec<u8>, ConnectionHandle>,
    handle: ConnectionHandle,
) -> HashSet<Vec<u8>> {
    cid_to_handle
        .iter()
        .filter(|(_, mapped_handle)| **mapped_handle == handle)
        .map(|(cid, _)| cid.clone())
        .collect()
}

fn trace_server_event(message: impl AsRef<str>) {
    if std::env::var_os("STARFISH_QUIC_TRACE").is_some() {
        eprintln!("starfish-quic-server: {}", message.as_ref());
    }
}

struct ConnectionUpdate {
    local_cids: Vec<Vec<u8>>,
    reset_tokens: Vec<(Vec<u8>, [u8; 16])>,
    established: bool,
    closed: bool,
}

impl ConnectionUpdate {
    fn capture(conn: &QuicConnection) -> Self {
        Self {
            local_cids: conn.local_cid_bytes(),
            reset_tokens: conn.local_cid_reset_tokens(),
            established: conn.is_established(),
            closed: matches!(conn.state(), crate::connection::ConnectionState::Closed),
        }
    }
}

fn version_negotiation_ids(parsed: &PacketHeader) -> Option<(ConnectionId, ConnectionId)> {
    match parsed {
        PacketHeader::Long(h) if h.version != 0 && h.version != packet::QUIC_VERSION_1 => {
            Some((h.scid.clone(), h.dcid.clone()))
        }
        _ => None,
    }
}

async fn send_version_negotiation(
    socket: &mut UdpSocket,
    peer_addr: SocketAddr,
    dcid: &ConnectionId,
    scid: &ConnectionId,
) -> Result<(), QuicError> {
    let mut packet = packet::version_negotiation::encode_version_negotiation(
        dcid,
        scid,
        &[packet::QUIC_VERSION_1],
    );
    socket.send_to(peer_addr, &mut packet).await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InitialTokenDecision {
    SendRetry,
    Drop,
    Proceed {
        original_dcid: ConnectionId,
        retry_source_connection_id: Option<ConnectionId>,
        peer_address_validated: bool,
    },
}

fn evaluate_initial_token(
    require_retry: bool,
    initial_token: &[u8],
    peer_addr: &SocketAddr,
    initial_dcid: &ConnectionId,
    retry_secret: &[u8; 32],
) -> Result<InitialTokenDecision, QuicError> {
    if initial_token.is_empty() {
        return Ok(if require_retry {
            InitialTokenDecision::SendRetry
        } else {
            InitialTokenDecision::Proceed {
                original_dcid: initial_dcid.clone(),
                retry_source_connection_id: None,
                peer_address_validated: false,
            }
        });
    }

    if require_retry {
        match retry::validate_retry_token(initial_token, peer_addr, retry_secret)? {
            retry::RetryTokenValidation::Valid(original_dcid) => {
                return Ok(InitialTokenDecision::Proceed {
                    original_dcid,
                    retry_source_connection_id: Some(initial_dcid.clone()),
                    peer_address_validated: true,
                });
            }
            retry::RetryTokenValidation::Invalid => return Ok(InitialTokenDecision::Drop),
            retry::RetryTokenValidation::Unknown => {}
        }
    }

    if retry::validate_new_token(initial_token, peer_addr, retry_secret)? {
        return Ok(InitialTokenDecision::Proceed {
            original_dcid: initial_dcid.clone(),
            retry_source_connection_id: None,
            peer_address_validated: true,
        });
    }

    Ok(if require_retry {
        InitialTokenDecision::SendRetry
    } else {
        InitialTokenDecision::Proceed {
            original_dcid: initial_dcid.clone(),
            retry_source_connection_id: None,
            peer_address_validated: false,
        }
    })
}

async fn send_retry_for_initial(
    socket: &mut UdpSocket,
    peer_addr: SocketAddr,
    client_dcid: &ConnectionId,
    client_scid: &ConnectionId,
    cid_len: u8,
    retry_secret: &[u8; 32],
) -> Result<(), QuicError> {
    let retry_cid = ConnectionId::generate(cid_len);
    let token = retry::encode_retry_token(&peer_addr, client_dcid, retry_secret);

    let mut retry_pkt = retry::encode_retry_packet(client_scid, &retry_cid, &token);
    let tag = retry::compute_retry_integrity_tag(client_dcid, &retry_pkt)?;
    retry_pkt.extend_from_slice(&tag);

    socket.send_to(peer_addr, &mut retry_pkt).await?;
    Ok(())
}

fn should_ignore_listener_connection_error(err: &QuicError) -> bool {
    matches!(
        err,
        QuicError::ConnectionDraining
            | QuicError::ConnectionClosed { .. }
            | QuicError::Transport(_, _)
            | QuicError::InvalidPacket(_)
            | QuicError::InvalidFrame(_)
            | QuicError::BufferTooSmall
            | QuicError::Tls(_)
    )
}

/// QUIC listener — accepts incoming connections on a UDP socket.
pub struct QuicListener {
    socket: UdpSocket,
    config: Arc<QuicServerConfig>,
}

impl QuicListener {
    /// Bind a QUIC listener to the given address.
    pub async fn bind(addr: SocketAddr, config: QuicServerConfig) -> Result<Self, QuicError> {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Self {
            socket,
            config: Arc::new(config),
        })
    }

    /// Returns the local socket address this listener is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Accept an incoming QUIC connection (single-connection model).
    ///
    /// Waits for an Initial packet from a client, creates a server-side
    /// connection, and drives the handshake to completion.
    ///
    /// For concurrent connections, use [`QuicEndpoint`] instead.
    pub async fn accept(&mut self) -> Result<QuicConnection, QuicError> {
        let mut recv_buf = vec![0u8; MAX_RECV_SIZE];

        'accept: loop {
            let (len, peer_addr, ecn) = self.socket.recv_from_with_ecn(&mut recv_buf).await?;
            let data = &recv_buf[..len];

            // Parse the header
            let cid_len = self.config.cid_len as usize;
            let parsed_header = match header::parse_header(data, cid_len) {
                Ok(h) => h,
                Err(_) => continue, // Skip malformed packets
            };

            if let Some((dcid, scid)) = version_negotiation_ids(&parsed_header) {
                send_version_negotiation(&mut self.socket, peer_addr, &dcid, &scid).await?;
                continue;
            }

            // Only accept Initial packets for new connections
            let (dcid, scid, initial_token) = match &parsed_header {
                PacketHeader::Long(ref h) if h.packet_type == LongPacketType::Initial => {
                    // Parse Initial fields to extract the token
                    let fields = crate::packet::initial::parse_initial_fields(
                        data,
                        h.header_len,
                        h.first_byte,
                    )
                    .ok();
                    let token = fields.map_or(&[][..], |f| f.token);
                    trace_server_event(format!(
                        "listener initial peer={} dcid_len={} scid_len={} token_len={}",
                        peer_addr,
                        h.dcid.len(),
                        h.scid.len(),
                        token.len()
                    ));
                    (h.dcid.clone(), h.scid.clone(), token)
                }
                _ => continue,
            };

            let (original_dcid, retry_source_connection_id, peer_address_validated) =
                match evaluate_initial_token(
                    self.config.require_retry,
                    initial_token,
                    &peer_addr,
                    &dcid,
                    &self.config.retry_secret,
                )? {
                    InitialTokenDecision::SendRetry => {
                        send_retry_for_initial(
                            &mut self.socket,
                            peer_addr,
                            &dcid,
                            &scid,
                            self.config.cid_len,
                            &self.config.retry_secret,
                        )
                        .await?;
                        continue;
                    }
                    InitialTokenDecision::Drop => continue,
                    InitialTokenDecision::Proceed {
                        original_dcid,
                        retry_source_connection_id,
                        peer_address_validated,
                    } => (
                        original_dcid,
                        retry_source_connection_id,
                        peer_address_validated,
                    ),
                };

            // Generate our local CID
            let local_cid = ConnectionId::generate(self.config.cid_len);
            let stateless_reset_token =
                (self.config.cid_len != 0).then(generate_stateless_reset_token);
            let original_dcid_len = original_dcid.len();
            let peer_scid_len = scid.len();

            // Create rustls QUIC server connection
            let mut transport_params = packet::LocalTransportParameters::new(
                DEFAULT_INITIAL_MAX_DATA,
                DEFAULT_INITIAL_MAX_STREAM_DATA,
                DEFAULT_INITIAL_MAX_STREAMS_BIDI,
                DEFAULT_INITIAL_MAX_STREAMS_UNI,
            );
            transport_params.max_idle_timeout_ms = DEFAULT_IDLE_TIMEOUT_MS;
            transport_params.stateless_reset_token = stateless_reset_token;
            transport_params.max_udp_payload_size = MAX_LOCAL_UDP_PAYLOAD_SIZE as u64;
            transport_params.original_destination_connection_id = Some(original_dcid.clone());
            transport_params.initial_source_connection_id = Some(local_cid.clone());
            transport_params.retry_source_connection_id = retry_source_connection_id.clone();

            let transport_params = packet::encode_transport_parameters(&transport_params)?;

            let tls = rustls::quic::ServerConnection::new(
                Arc::new(self.config.tls_config.clone()),
                rustls::quic::Version::V1,
                transport_params.clone(),
            )
            .map_err(QuicError::Tls)?;

            // Clone the listener socket so the connection responds from the same address.
            let conn_socket = self.socket.try_clone()?;

            let mut conn = QuicConnection::new_server(
                conn_socket,
                peer_addr,
                local_cid,
                scid,
                tls,
                dcid.as_slice(),
                original_dcid,
                retry_source_connection_id,
                Some(self.config.retry_secret),
                peer_address_validated,
            )?;
            if let Some(token) = stateless_reset_token {
                conn.set_primary_local_stateless_reset_token(token);
            }
            trace_server_event(format!(
                "listener created connection peer={} original_dcid_len={} peer_scid_len={}",
                peer_addr, original_dcid_len, peer_scid_len
            ));

            // Feed the received Initial packet data to the connection.
            if let Err(err) = conn.feed_datagram_from_with_ecn(data, peer_addr, ecn) {
                if should_ignore_listener_connection_error(&err) {
                    continue;
                }
                return Err(err);
            }

            // Drive the handshake.
            if let Err(err) = conn.process_tls_output() {
                if should_ignore_listener_connection_error(&err) {
                    continue;
                }
                return Err(err);
            }
            if let Err(err) = conn.send_pending_frames().await {
                if should_ignore_listener_connection_error(&err) {
                    continue;
                }
                return Err(err);
            }

            // Continue polling until handshake completes.
            while !conn.is_established() {
                match conn.poll().await {
                    Ok(()) => {}
                    Err(err) => {
                        if should_ignore_listener_connection_error(&err) {
                            continue 'accept;
                        }
                        return Err(err);
                    }
                }
            }

            return Ok(conn);
        }
    }
}

// ---- QUIC-6: Multi-connection server endpoint ----

/// Opaque handle to a connection managed by [`QuicEndpoint`].
pub type ConnectionHandle = u64;

/// Multi-connection QUIC server endpoint with DCID-based datagram routing.
///
/// Unlike [`QuicListener`] which handles one connection at a time, `QuicEndpoint`
/// owns the UDP socket and routes incoming datagrams to the correct connection
/// by Destination Connection ID. Connections are endpoint-managed — they don't
/// read from the socket directly; the endpoint dispatches datagrams to them.
///
/// # Usage
/// ```ignore
/// let mut endpoint = QuicEndpoint::bind(addr, config).await?;
/// loop {
///     let handle = endpoint.accept().await?;
///     // Use endpoint.stream_recv / stream_send / open_bidi_stream with the handle
///     endpoint.drive().await?; // must be called to process I/O for all connections
/// }
/// ```
pub struct QuicEndpoint {
    socket: UdpSocket,
    config: Arc<QuicServerConfig>,
    /// All managed connections, keyed by handle.
    connections: HashMap<ConnectionHandle, QuicConnection>,
    /// Maps local CID bytes → connection handle for routing received datagrams.
    cid_to_handle: HashMap<Vec<u8>, ConnectionHandle>,
    /// Maps initial DCID (before CID is assigned) → handle for handshaking connections.
    initial_dcid_to_handle: HashMap<Vec<u8>, ConnectionHandle>,
    /// Stateless reset tokens indexed by local CID bytes.
    cid_to_reset_token: HashMap<Vec<u8>, [u8; 16]>,
    next_handle: ConnectionHandle,
    /// Handles of connections that completed the handshake and are ready for accept().
    established_queue: VecDeque<ConnectionHandle>,
    established_set: HashSet<ConnectionHandle>,
    recv_buf: Vec<u8>,
}

impl QuicEndpoint {
    fn next_connection_deadline(&self) -> Option<Instant> {
        self.connections
            .values()
            .filter_map(QuicConnection::next_internal_deadline)
            .min()
    }

    fn apply_connection_update(&mut self, handle: ConnectionHandle, update: ConnectionUpdate) {
        if update.closed {
            self.remove_connection(handle);
            return;
        }

        let previous_cids = cids_for_handle(&self.cid_to_handle, handle);
        let current_cids: HashSet<Vec<u8>> = update.local_cids.iter().cloned().collect();
        self.cid_to_handle
            .retain(|cid, mapped_handle| *mapped_handle != handle || current_cids.contains(cid));
        self.cid_to_reset_token
            .retain(|cid, _| !previous_cids.contains(cid) || current_cids.contains(cid));

        for cid in update.local_cids {
            self.cid_to_handle.insert(cid, handle);
        }
        for (cid, token) in update.reset_tokens {
            self.cid_to_reset_token.insert(cid, token);
        }
        if update.established {
            queue_established_handle(
                &mut self.established_queue,
                &mut self.established_set,
                handle,
            );
        }
    }

    fn drop_failing_connection(&mut self, handle: ConnectionHandle, context: &str, err: QuicError) {
        trace_server_event(format!(
            "{context} connection handle={handle} dropped after error: {err}"
        ));
        self.remove_connection(handle);
    }

    async fn flush_connection(&mut self, handle: ConnectionHandle, context: &str) {
        let result = match self.connections.get_mut(&handle) {
            Some(conn) => match conn.flush().await {
                Ok(()) => Ok(ConnectionUpdate::capture(conn)),
                Err(err) => Err(err),
            },
            None => return,
        };

        match result {
            Ok(update) => {
                self.apply_connection_update(handle, update);
            }
            Err(err) => {
                self.drop_failing_connection(handle, context, err);
            }
        }
    }

    async fn route_datagram_to_connection(
        &mut self,
        handle: ConnectionHandle,
        data: &[u8],
        peer_addr: SocketAddr,
        ecn: Option<UdpEcnCodepoint>,
    ) {
        let result = match self.connections.get_mut(&handle) {
            Some(conn) => {
                let feed_result = conn.feed_datagram_from_with_ecn(data, peer_addr, ecn);
                match feed_result {
                    Ok(()) => match conn.flush().await {
                        Ok(()) => Ok(ConnectionUpdate::capture(conn)),
                        Err(err) => Err(err),
                    },
                    Err(err) => Err(err),
                }
            }
            None => return,
        };

        match result {
            Ok(update) => self.apply_connection_update(handle, update),
            Err(err) => self.drop_failing_connection(handle, "routing datagram", err),
        }
    }

    fn effective_recv_timeout(&self) -> Option<IOTimeout> {
        let deadline = self.next_connection_deadline()?;
        let now_instant = Instant::now();
        let now_system = SystemTime::now();
        Some(IOTimeout::at_time(
            now_system + deadline.saturating_duration_since(now_instant),
        ))
    }

    async fn handle_connection_timeouts(&mut self, now: Instant) {
        let handles: Vec<ConnectionHandle> = self.connections.keys().copied().collect();
        let mut closed_handles = Vec::new();

        for handle in handles {
            let should_flush = if let Some(conn) = self.connections.get_mut(&handle) {
                conn.handle_internal_timers(now);
                if matches!(conn.state(), crate::connection::ConnectionState::Closed) {
                    closed_handles.push(handle);
                    false
                } else {
                    !matches!(conn.state(), crate::connection::ConnectionState::Draining)
                }
            } else {
                false
            };

            if should_flush {
                self.flush_connection(handle, "flushing timed connection")
                    .await;
            }
        }

        for handle in closed_handles {
            self.remove_connection(handle);
        }
    }

    /// Bind a QUIC endpoint to the given address.
    pub async fn bind(addr: SocketAddr, config: QuicServerConfig) -> Result<Self, QuicError> {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Self {
            socket,
            config: Arc::new(config),
            connections: HashMap::new(),
            cid_to_handle: HashMap::new(),
            initial_dcid_to_handle: HashMap::new(),
            cid_to_reset_token: HashMap::new(),
            next_handle: 0,
            established_queue: VecDeque::new(),
            established_set: HashSet::new(),
            recv_buf: vec![0u8; MAX_RECV_SIZE],
        })
    }

    /// Accept a new fully-handshaked connection. Blocks until one is ready.
    pub async fn accept(&mut self) -> Result<ConnectionHandle, QuicError> {
        loop {
            if let Some(handle) =
                dequeue_established_handle(&mut self.established_queue, &mut self.established_set)
            {
                return Ok(handle);
            }
            self.drive_once().await?;
        }
    }

    /// Receive one datagram and route it; process pending sends for all connections.
    pub async fn drive(&mut self) -> Result<(), QuicError> {
        self.drive_once().await
    }

    /// Internal: receive one datagram, route, and flush all connections.
    async fn drive_once(&mut self) -> Result<(), QuicError> {
        self.handle_connection_timeouts(Instant::now()).await;

        let recv_timeout = self.effective_recv_timeout();
        let (len, peer_addr, ecn) = match self
            .socket
            .recv_from_with_timeout_and_ecn(&mut self.recv_buf, &recv_timeout)
            .await
        {
            Ok(result) => result,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                self.handle_connection_timeouts(Instant::now()).await;
                return Ok(());
            }
            Err(e) => return Err(QuicError::Io(e)),
        };
        let data = self.recv_buf[..len].to_vec();

        let cid_len = self.config.cid_len as usize;
        let parsed = match header::parse_header(&data, cid_len) {
            Ok(h) => h,
            Err(_) => return Ok(()),
        };

        if let Some((dcid, scid)) = version_negotiation_ids(&parsed) {
            send_version_negotiation(&mut self.socket, peer_addr, &dcid, &scid).await?;
            return Ok(());
        }

        // Extract the received DCID (which is our local CID)
        let dcid_bytes = match &parsed {
            PacketHeader::Long(h) => h.dcid.as_slice().to_vec(),
            PacketHeader::Short(h) => h.dcid.as_slice().to_vec(),
            PacketHeader::VersionNegotiation { .. } => return Ok(()),
        };

        // Route to existing connection by local CID
        if let Some(&handle) = self.cid_to_handle.get(&dcid_bytes) {
            self.route_datagram_to_connection(handle, &data, peer_addr, ecn)
                .await;
            return Ok(());
        }

        // Route by initial DCID (for handshaking connections that haven't switched CIDs yet)
        if let Some(&handle) = self.initial_dcid_to_handle.get(&dcid_bytes) {
            self.route_datagram_to_connection(handle, &data, peer_addr, ecn)
                .await;
            return Ok(());
        }

        if let Some(token) = self.cid_to_reset_token.get(&dcid_bytes).copied() {
            if let Some(mut packet) = QuicConnection::encode_stateless_reset(token, data.len()) {
                self.socket.send_to(peer_addr, &mut packet).await?;
            }
            return Ok(());
        }

        // New connection — must be an Initial packet
        let (initial_dcid, scid, initial_token) = match &parsed {
            PacketHeader::Long(h) if h.packet_type == LongPacketType::Initial => {
                let fields =
                    crate::packet::initial::parse_initial_fields(&data, h.header_len, h.first_byte)
                        .ok();
                let token = fields.map_or(&[][..], |f| f.token);
                (h.dcid.clone(), h.scid.clone(), token)
            }
            _ => return Ok(()),
        };

        let (original_dcid, retry_source_connection_id, peer_address_validated) =
            match evaluate_initial_token(
                self.config.require_retry,
                initial_token,
                &peer_addr,
                &initial_dcid,
                &self.config.retry_secret,
            )? {
                InitialTokenDecision::SendRetry => {
                    send_retry_for_initial(
                        &mut self.socket,
                        peer_addr,
                        &initial_dcid,
                        &scid,
                        self.config.cid_len,
                        &self.config.retry_secret,
                    )
                    .await?;
                    return Ok(());
                }
                InitialTokenDecision::Drop => return Ok(()),
                InitialTokenDecision::Proceed {
                    original_dcid,
                    retry_source_connection_id,
                    peer_address_validated,
                } => (
                    original_dcid,
                    retry_source_connection_id,
                    peer_address_validated,
                ),
            };

        let local_cid = ConnectionId::generate(self.config.cid_len);
        let local_cid_bytes = local_cid.as_slice().to_vec();
        let stateless_reset_token = (self.config.cid_len != 0).then(generate_stateless_reset_token);

        let mut transport_params = packet::LocalTransportParameters::new(
            DEFAULT_INITIAL_MAX_DATA,
            DEFAULT_INITIAL_MAX_STREAM_DATA,
            DEFAULT_INITIAL_MAX_STREAMS_BIDI,
            DEFAULT_INITIAL_MAX_STREAMS_UNI,
        );
        transport_params.max_idle_timeout_ms = DEFAULT_IDLE_TIMEOUT_MS;
        transport_params.stateless_reset_token = stateless_reset_token;
        transport_params.max_udp_payload_size = MAX_LOCAL_UDP_PAYLOAD_SIZE as u64;
        transport_params.original_destination_connection_id = Some(original_dcid.clone());
        transport_params.initial_source_connection_id = Some(local_cid.clone());
        transport_params.retry_source_connection_id = retry_source_connection_id.clone();

        let transport_params = packet::encode_transport_parameters(&transport_params)?;

        let tls = rustls::quic::ServerConnection::new(
            Arc::new(self.config.tls_config.clone()),
            rustls::quic::Version::V1,
            transport_params.clone(),
        )
        .map_err(QuicError::Tls)?;

        let conn_socket = self.socket.try_clone()?;

        let mut conn = QuicConnection::new_server(
            conn_socket,
            peer_addr,
            local_cid,
            scid,
            tls,
            initial_dcid.as_slice(),
            original_dcid,
            retry_source_connection_id,
            Some(self.config.retry_secret),
            peer_address_validated,
        )?;
        if let Some(token) = stateless_reset_token {
            conn.set_primary_local_stateless_reset_token(token);
        }

        conn.endpoint_managed = true;
        if let Err(err) = conn.feed_datagram_from_with_ecn(&data, peer_addr, ecn) {
            trace_server_event(format!(
                "dropping new connection from {peer_addr} after receive error: {err}"
            ));
            return Ok(());
        }
        if let Err(err) = conn.flush().await {
            trace_server_event(format!(
                "dropping new connection from {peer_addr} after flush error: {err}"
            ));
            return Ok(());
        }

        let handle = self.next_handle;
        self.next_handle += 1;

        self.cid_to_handle.insert(local_cid_bytes, handle);
        register_connection_cids(&mut self.cid_to_handle, handle, &conn);
        register_connection_reset_tokens(&mut self.cid_to_reset_token, &conn);
        self.initial_dcid_to_handle
            .insert(initial_dcid.as_slice().to_vec(), handle);

        if conn.is_established() {
            queue_established_handle(
                &mut self.established_queue,
                &mut self.established_set,
                handle,
            );
        }
        self.connections.insert(handle, conn);

        Ok(())
    }

    // ---- Delegated stream API ----

    /// Open a new bidirectional stream on a connection.
    pub fn open_bidi_stream(&mut self, handle: ConnectionHandle) -> Result<StreamId, QuicError> {
        self.get_conn_mut(handle)?.open_bidi_stream()
    }

    /// Open a new unidirectional stream on a connection.
    pub fn open_uni_stream(&mut self, handle: ConnectionHandle) -> Result<StreamId, QuicError> {
        self.get_conn_mut(handle)?.open_uni_stream()
    }

    /// Write data to a stream.
    pub fn stream_send(
        &mut self,
        handle: ConnectionHandle,
        stream_id: StreamId,
        data: &[u8],
        fin: bool,
    ) -> Result<usize, QuicError> {
        self.get_conn_mut(handle)?.stream_send(stream_id, data, fin)
    }

    /// Read data from a stream.
    pub fn stream_recv(
        &mut self,
        handle: ConnectionHandle,
        stream_id: StreamId,
        buf: &mut [u8],
    ) -> Result<(usize, bool), QuicError> {
        self.get_conn_mut(handle)?.stream_recv(stream_id, buf)
    }

    /// Find an incoming bidirectional stream with data.
    pub fn accept_incoming_bidi(&self, handle: ConnectionHandle) -> Option<StreamId> {
        self.connections.get(&handle)?.accept_incoming_bidi()
    }

    /// Find an incoming unidirectional stream with data.
    pub fn accept_incoming_uni(&self, handle: ConnectionHandle) -> Option<StreamId> {
        self.connections.get(&handle)?.accept_incoming_uni()
    }

    /// Snapshot incoming unidirectional streams that currently have data buffered.
    pub fn incoming_uni_stream_ids(&self, handle: ConnectionHandle) -> Vec<StreamId> {
        self.connections
            .get(&handle)
            .map(QuicConnection::incoming_uni_stream_ids)
            .unwrap_or_default()
    }

    /// Close a connection.
    pub async fn close_connection(
        &mut self,
        handle: ConnectionHandle,
        error_code: u64,
        reason: &[u8],
    ) -> Result<(), QuicError> {
        let close_result = match self.connections.get_mut(&handle) {
            Some(conn) => conn.close(error_code, reason).await,
            None => return Ok(()),
        };

        if let Err(err) = close_result {
            trace_server_event(format!(
                "closing connection handle={handle} failed; dropping handle: {err}"
            ));
            self.remove_connection(handle);
            return Err(err);
        }
        unqueue_established_handle(
            &mut self.established_queue,
            &mut self.established_set,
            handle,
        );
        Ok(())
    }

    /// Send pending frames for all managed connections.
    ///
    /// Per-connection flush failures are isolated to the failing handle and do
    /// not abort the endpoint-wide pass.
    pub async fn flush_all(&mut self) {
        let handles: Vec<ConnectionHandle> = self.connections.keys().copied().collect();
        for handle in handles {
            self.flush_connection(handle, "flushing all managed connections")
                .await;
        }
    }

    fn get_conn_mut(&mut self, handle: ConnectionHandle) -> Result<&mut QuicConnection, QuicError> {
        self.connections.get_mut(&handle).ok_or_else(|| {
            QuicError::Transport(
                crate::error::TransportErrorCode::InternalError,
                format!("unknown connection handle {handle}"),
            )
        })
    }

    fn remove_connection(&mut self, handle: ConnectionHandle) {
        let previous_cids = cids_for_handle(&self.cid_to_handle, handle);
        self.connections.remove(&handle);
        self.cid_to_handle.retain(|_, h| *h != handle);
        self.initial_dcid_to_handle.retain(|_, h| *h != handle);
        self.cid_to_reset_token
            .retain(|cid, _| !previous_cids.contains(cid));
        unqueue_established_handle(
            &mut self.established_queue,
            &mut self.established_set,
            handle,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dequeue_established_handle, evaluate_initial_token, queue_established_handle,
        ConnectionHandle, ConnectionUpdate, InitialTokenDecision, QuicEndpoint, QuicServerConfig,
    };
    use crate::connection::{ConnectionState, QuicConnection};
    use crate::error::QuicError;
    use crate::frame::Frame;
    use crate::packet::header::{self, LongPacketType};
    use crate::packet::{self, retry, ConnectionId};
    use crate::transport::stream_manager::RecvState;
    use rcgen::{CertifiedKey, KeyPair};
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::ServerConfig;
    use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
    use starfish_reactor::reactor::Reactor;
    use std::collections::{HashSet, VecDeque};
    use std::net::SocketAddr;
    use std::sync::Arc;

    #[test]
    fn established_queue_side_index_prevents_duplicates() {
        let mut queue = VecDeque::new();
        let mut queued = HashSet::new();

        queue_established_handle(&mut queue, &mut queued, 7);
        queue_established_handle(&mut queue, &mut queued, 7);

        assert_eq!(queue.len(), 1);
        assert_eq!(dequeue_established_handle(&mut queue, &mut queued), Some(7));
        assert!(queue.is_empty());
        assert!(queued.is_empty());
    }

    #[test]
    fn established_handle_can_be_reannounced_after_dequeue() {
        let mut queue = VecDeque::new();
        let mut announced = HashSet::new();

        queue_established_handle(&mut queue, &mut announced, 7);
        assert_eq!(
            dequeue_established_handle(&mut queue, &mut announced),
            Some(7)
        );

        queue_established_handle(&mut queue, &mut announced, 7);
        assert_eq!(
            dequeue_established_handle(&mut queue, &mut announced),
            Some(7)
        );
    }

    #[test]
    fn evaluate_initial_token_requests_retry_when_required_and_missing() {
        let peer_addr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let initial_dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let secret = [0x11; 32];

        let decision =
            evaluate_initial_token(true, &[], &peer_addr, &initial_dcid, &secret).unwrap();
        assert!(matches!(decision, InitialTokenDecision::SendRetry));
    }

    #[test]
    fn evaluate_initial_token_accepts_new_token_when_retry_required() {
        let peer_addr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let initial_dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let secret = [0x22; 32];
        let token = retry::encode_new_token(&peer_addr, &secret);

        let decision =
            evaluate_initial_token(true, &token, &peer_addr, &initial_dcid, &secret).unwrap();
        match decision {
            InitialTokenDecision::Proceed {
                original_dcid,
                retry_source_connection_id,
                peer_address_validated,
            } => {
                assert_eq!(original_dcid, initial_dcid);
                assert!(retry_source_connection_id.is_none());
                assert!(peer_address_validated);
            }
            other => panic!("expected validated NEW_TOKEN decision, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_initial_token_drops_identifiable_invalid_retry_token() {
        let peer_addr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let wrong_addr: SocketAddr = "127.0.0.2:4433".parse().unwrap();
        let initial_dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let secret = [0x33; 32];
        let token = retry::encode_retry_token(&wrong_addr, &initial_dcid, &secret);

        let decision =
            evaluate_initial_token(true, &token, &peer_addr, &initial_dcid, &secret).unwrap();
        assert!(matches!(decision, InitialTokenDecision::Drop));
    }

    #[test]
    fn evaluate_initial_token_proceeds_unvalidated_without_retry() {
        let peer_addr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let initial_dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let secret = [0x44; 32];

        let decision =
            evaluate_initial_token(false, &[], &peer_addr, &initial_dcid, &secret).unwrap();
        match decision {
            InitialTokenDecision::Proceed {
                original_dcid,
                retry_source_connection_id,
                peer_address_validated,
            } => {
                assert_eq!(original_dcid, initial_dcid);
                assert!(retry_source_connection_id.is_none());
                assert!(!peer_address_validated);
            }
            other => panic!("expected unvalidated proceed decision, got {other:?}"),
        }
    }

    fn generate_self_signed_cert() -> Result<CertifiedKey, rcgen::Error> {
        let subject_alt_names = vec!["localhost".to_string()];
        let mut params = rcgen::CertificateParams::new(subject_alt_names)?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "localhost");

        let key_pair = KeyPair::generate()?;
        let cert = params.self_signed(&key_pair)?;
        Ok(CertifiedKey { cert, key_pair })
    }

    fn make_server_config() -> QuicServerConfig {
        let certified = generate_self_signed_cert().unwrap();
        let key_der =
            PrivateKeyDer::from(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
        let tls_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key_der)
            .unwrap();
        QuicServerConfig::new(tls_config)
    }

    fn make_managed_server_connection(
        endpoint: &QuicEndpoint,
        peer_addr: SocketAddr,
        seed: u8,
    ) -> Result<QuicConnection, String> {
        let local_cid = ConnectionId::from_slice(&[seed; 8]);
        let remote_cid = ConnectionId::from_slice(&[seed.wrapping_add(1); 8]);
        let original_dcid = ConnectionId::from_slice(&[seed.wrapping_add(2); 8]);
        let original_dcid_bytes = original_dcid.as_slice().to_vec();

        let mut transport_params = packet::LocalTransportParameters::new(
            crate::connection::DEFAULT_INITIAL_MAX_DATA,
            crate::connection::DEFAULT_INITIAL_MAX_STREAM_DATA,
            crate::connection::DEFAULT_INITIAL_MAX_STREAMS_BIDI,
            crate::connection::DEFAULT_INITIAL_MAX_STREAMS_UNI,
        );
        transport_params.max_idle_timeout_ms = crate::connection::DEFAULT_IDLE_TIMEOUT_MS;
        transport_params.max_udp_payload_size =
            crate::connection::MAX_LOCAL_UDP_PAYLOAD_SIZE as u64;

        let transport_params =
            packet::encode_transport_parameters(&transport_params).map_err(|e| e.to_string())?;
        let tls = rustls::quic::ServerConnection::new(
            Arc::new(endpoint.config.tls_config.clone()),
            rustls::quic::Version::V1,
            transport_params,
        )
        .map_err(|e| e.to_string())?;

        let mut conn = QuicConnection::new_server(
            endpoint.socket.try_clone().map_err(|e| e.to_string())?,
            peer_addr,
            local_cid,
            remote_cid,
            tls,
            &original_dcid_bytes,
            original_dcid,
            None,
            Some(endpoint.config.retry_secret),
            true,
        )
        .map_err(|e| e.to_string())?;
        conn.endpoint_managed = true;
        Ok(conn)
    }

    fn truncated_initial_for_dcid(dcid: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();
        header::encode_long_header(
            LongPacketType::Initial,
            &ConnectionId::from_slice(dcid),
            &ConnectionId::from_slice(&[0x42; 8]),
            1,
            &mut packet,
        );
        packet
    }

    #[test]
    fn endpoint_drive_isolates_connection_routing_errors() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut reactor = Reactor::new();
        let task = reactor.spawn_with_result(async move {
            let mut endpoint =
                QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), make_server_config())
                    .await
                    .map_err(|e| e.to_string())?;
            let addr = endpoint.socket.local_addr().map_err(|e| e.to_string())?;

            let bad_handle: ConnectionHandle = 7;
            let good_handle: ConnectionHandle = 8;
            let bad_conn = make_managed_server_connection(
                &endpoint,
                "127.0.0.1:44431".parse().unwrap(),
                0x10,
            )?;
            let bad_dcid = bad_conn.local_cid_bytes()[0].clone();
            endpoint.connections.insert(bad_handle, bad_conn);
            endpoint.cid_to_handle.insert(bad_dcid.clone(), bad_handle);

            let good_conn = make_managed_server_connection(
                &endpoint,
                "127.0.0.1:44432".parse().unwrap(),
                0x20,
            )?;
            let good_dcid = good_conn.local_cid_bytes()[0].clone();
            endpoint.connections.insert(good_handle, good_conn);
            endpoint.cid_to_handle.insert(good_dcid, good_handle);

            let sender = std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
            sender
                .send_to(&truncated_initial_for_dcid(&bad_dcid), addr)
                .map_err(|e| e.to_string())?;

            endpoint.drive().await.map_err(|e| e.to_string())?;

            assert!(!endpoint.connections.contains_key(&bad_handle));
            assert!(endpoint.connections.contains_key(&good_handle));
            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }

    #[test]
    fn close_connection_keeps_managed_connection_routable_while_closing() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut reactor = Reactor::new();
        let task = reactor.spawn_with_result(async move {
            let mut endpoint =
                QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), make_server_config())
                    .await
                    .map_err(|e| e.to_string())?;

            let handle: ConnectionHandle = 11;
            let conn = make_managed_server_connection(
                &endpoint,
                "127.0.0.1:44433".parse().unwrap(),
                0x30,
            )?;
            let local_cid = conn.local_cid_bytes()[0].clone();
            endpoint.connections.insert(handle, conn);
            endpoint.cid_to_handle.insert(local_cid.clone(), handle);
            queue_established_handle(
                &mut endpoint.established_queue,
                &mut endpoint.established_set,
                handle,
            );

            endpoint
                .close_connection(handle, 0x1234, b"bye")
                .await
                .map_err(|e| e.to_string())?;

            let conn = endpoint
                .connections
                .get(&handle)
                .ok_or_else(|| "connection removed too early".to_string())?;
            assert_eq!(conn.state(), ConnectionState::Closing);
            assert_eq!(endpoint.cid_to_handle.get(&local_cid), Some(&handle));
            assert!(!endpoint.established_set.contains(&handle));
            assert!(endpoint.established_queue.is_empty());
            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }

    #[test]
    fn delegated_stream_api_handles_uni_send_and_receive() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut reactor = Reactor::new();
        let task = reactor.spawn_with_result(async move {
            let mut endpoint =
                QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), make_server_config())
                    .await
                    .map_err(|e| e.to_string())?;

            let handle: ConnectionHandle = 21;
            let mut conn = make_managed_server_connection(
                &endpoint,
                "127.0.0.1:44434".parse().unwrap(),
                0x50,
            )?;
            let provider = rustls::crypto::ring::default_provider();
            let keys = crate::crypto::keys::derive_initial_keys(
                &[1, 2, 3, 4, 5, 6, 7, 8],
                true,
                &provider,
            )
            .map_err(|e| e.to_string())?;
            conn.keys.set_from_quic_keys(
                crate::packet::header::PacketNumberSpace::ApplicationData,
                keys,
            );
            conn.state = ConnectionState::Connected;
            endpoint.connections.insert(handle, conn);

            let local_uni = endpoint
                .open_uni_stream(handle)
                .map_err(|e| e.to_string())?;
            assert_eq!(local_uni, 3);
            assert_eq!(
                endpoint
                    .stream_send(handle, local_uni, b"ctrl", true)
                    .map_err(|e| e.to_string())?,
                4
            );

            let sent = endpoint
                .connections
                .get(&handle)
                .and_then(|conn| conn.pending_frames.back())
                .map(|(_, frame)| frame.clone())
                .ok_or_else(|| "missing delegated send frame".to_string())?;
            match sent {
                Frame::Stream(frame) => {
                    assert_eq!(frame.stream_id, local_uni);
                    assert!(frame.fin);
                    assert_eq!(frame.data, b"ctrl".to_vec());
                }
                other => return Err(format!("expected STREAM frame, got {other:?}")),
            }

            let remote_uni = 2;
            let conn = endpoint
                .connections
                .get_mut(&handle)
                .ok_or_else(|| "missing managed connection".to_string())?;
            conn.streams
                .accept_stream(remote_uni)
                .map_err(|e| e.to_string())?;
            let stream = conn
                .streams
                .get_mut(remote_uni)
                .ok_or_else(|| "missing remote unidirectional stream".to_string())?;
            stream.recv_buf.insert(0, b"peer".to_vec());
            stream.final_size = Some(4);
            stream.recv = Some(RecvState::SizeKnown);

            assert_eq!(endpoint.accept_incoming_uni(handle), Some(remote_uni));
            assert_eq!(endpoint.incoming_uni_stream_ids(handle), vec![remote_uni]);

            let mut buf = [0u8; 8];
            let (n, fin) = endpoint
                .stream_recv(handle, remote_uni, &mut buf)
                .map_err(|e| e.to_string())?;
            assert_eq!(&buf[..n], b"peer");
            assert!(fin);
            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }

    #[test]
    fn close_connection_drops_handle_when_close_send_fails() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut reactor = Reactor::new();
        let task = reactor.spawn_with_result(async move {
            let mut endpoint =
                QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), make_server_config())
                    .await
                    .map_err(|e| e.to_string())?;

            let handle: ConnectionHandle = 12;
            let bad_peer: SocketAddr = "[::1]:4433".parse().unwrap();
            let conn = make_managed_server_connection(&endpoint, bad_peer, 0x40)?;
            let local_cid = conn.local_cid_bytes()[0].clone();
            endpoint.connections.insert(handle, conn);
            endpoint.cid_to_handle.insert(local_cid.clone(), handle);
            queue_established_handle(
                &mut endpoint.established_queue,
                &mut endpoint.established_set,
                handle,
            );

            let err = endpoint
                .close_connection(handle, 0x1234, b"bye")
                .await
                .expect_err("close should fail on IPv4 socket -> IPv6 peer");
            assert!(matches!(err, QuicError::Io(_)));
            assert!(!endpoint.connections.contains_key(&handle));
            assert!(!endpoint.cid_to_handle.contains_key(&local_cid));
            assert!(!endpoint.established_set.contains(&handle));
            assert!(endpoint.established_queue.is_empty());
            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }

    #[test]
    fn apply_connection_update_prunes_retired_cid_routes_and_tokens() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut reactor = Reactor::new();
        let task = reactor.spawn_with_result(async move {
            let mut endpoint =
                QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), make_server_config())
                    .await
                    .map_err(|e| e.to_string())?;

            let handle: ConnectionHandle = 31;
            let retired_cid = vec![0x31; 8];
            let active_cid = vec![0x32; 8];
            let active_token = [0x32; 16];

            endpoint.cid_to_handle.insert(retired_cid.clone(), handle);
            endpoint
                .cid_to_reset_token
                .insert(retired_cid.clone(), [0x31; 16]);

            endpoint.apply_connection_update(
                handle,
                ConnectionUpdate {
                    local_cids: vec![active_cid.clone()],
                    reset_tokens: vec![(active_cid.clone(), active_token)],
                    established: false,
                    closed: false,
                },
            );

            assert!(!endpoint.cid_to_handle.contains_key(&retired_cid));
            assert!(!endpoint.cid_to_reset_token.contains_key(&retired_cid));
            assert_eq!(endpoint.cid_to_handle.get(&active_cid), Some(&handle));
            assert_eq!(
                endpoint.cid_to_reset_token.get(&active_cid),
                Some(&active_token)
            );
            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }

    #[test]
    fn remove_connection_clears_stateless_reset_tokens() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut reactor = Reactor::new();
        let task = reactor.spawn_with_result(async move {
            let mut endpoint =
                QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), make_server_config())
                    .await
                    .map_err(|e| e.to_string())?;

            let handle: ConnectionHandle = 32;
            let local_cid = vec![0x40; 8];
            endpoint.cid_to_handle.insert(local_cid.clone(), handle);
            endpoint
                .cid_to_reset_token
                .insert(local_cid.clone(), [0x40; 16]);

            endpoint.remove_connection(handle);

            assert!(!endpoint.cid_to_handle.contains_key(&local_cid));
            assert!(!endpoint.cid_to_reset_token.contains_key(&local_cid));
            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }
}
