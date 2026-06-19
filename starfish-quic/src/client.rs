//! QUIC client — initiates connections to a remote QUIC server.
//!
//! [`QuicClient`] handles connection setup including UDP socket binding,
//! TLS configuration, and Initial packet exchange.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use rustls::ClientConfig;
use starfish_reactor::cooperative_io::udp_socket::UdpSocket;

use crate::connection::{
    QuicConnection, DEFAULT_IDLE_TIMEOUT_MS, DEFAULT_INITIAL_MAX_DATA,
    DEFAULT_INITIAL_MAX_STREAMS_BIDI, DEFAULT_INITIAL_MAX_STREAMS_UNI,
    DEFAULT_INITIAL_MAX_STREAM_DATA, MAX_LOCAL_UDP_PAYLOAD_SIZE,
};
use crate::error::QuicError;
use crate::packet::{self, ConnectionId};

/// Configuration for a QUIC client connection.
#[derive(Clone)]
pub struct QuicClientConfig {
    /// TLS client configuration.
    pub tls_config: ClientConfig,
    /// Connection ID length (default: 8 bytes).
    pub cid_len: u8,
    peer_transport_parameter_cache:
        Arc<Mutex<std::collections::HashMap<String, packet::PeerTransportParameters>>>,
    token_cache: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

impl QuicClientConfig {
    /// Create a new QuicClientConfig with the given TLS config.
    pub fn new(tls_config: ClientConfig) -> Self {
        Self {
            tls_config,
            cid_len: 8,
            peer_transport_parameter_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            token_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Enable TLS 1.3 early data on resumed connections.
    pub fn enable_early_data(mut self) -> Self {
        self.tls_config.enable_early_data = true;
        self
    }
}

/// QUIC client — establishes connections to remote servers.
pub struct QuicClient;

impl QuicClient {
    /// Connect to a remote QUIC server.
    ///
    /// Performs the QUIC handshake (Initial + Handshake packets) and returns
    /// an established [`QuicConnection`].
    pub async fn connect(
        addr: SocketAddr,
        server_name: &str,
        config: QuicClientConfig,
    ) -> Result<QuicConnection, QuicError> {
        let mut conn = Self::connect_start(addr, server_name, config).await?;

        // Poll until connected (handshake complete)
        while !conn.is_established() {
            conn.poll().await?;
        }

        Ok(conn)
    }

    /// Start connecting to a remote QUIC server without waiting for handshake completion.
    ///
    /// This sends the Initial flight and returns immediately so callers can
    /// queue 0-RTT data on resumed connections.
    pub async fn connect_start(
        addr: SocketAddr,
        server_name: &str,
        config: QuicClientConfig,
    ) -> Result<QuicConnection, QuicError> {
        // Bind a UDP socket to any available port
        let bind_addr: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let socket = UdpSocket::bind(bind_addr).await?;

        // Generate connection IDs
        let local_cid = ConnectionId::generate(config.cid_len);
        let remote_cid = ConnectionId::generate(config.cid_len);

        // Create rustls QUIC client connection
        let cache_key = server_name.to_string();
        let server_name = rustls::pki_types::ServerName::try_from(cache_key.clone())
            .map_err(|e| QuicError::InvalidPacket(format!("invalid server name: {e}")))?;

        let mut transport_params = packet::LocalTransportParameters::new(
            DEFAULT_INITIAL_MAX_DATA,
            DEFAULT_INITIAL_MAX_STREAM_DATA,
            DEFAULT_INITIAL_MAX_STREAMS_BIDI,
            DEFAULT_INITIAL_MAX_STREAMS_UNI,
        );
        transport_params.max_idle_timeout_ms = DEFAULT_IDLE_TIMEOUT_MS;
        transport_params.max_udp_payload_size = MAX_LOCAL_UDP_PAYLOAD_SIZE as u64;
        transport_params.initial_source_connection_id = Some(local_cid.clone());

        let transport_params = packet::encode_transport_parameters(&transport_params)?;

        let tls = rustls::quic::ClientConnection::new(
            Arc::new(config.tls_config),
            rustls::quic::Version::V1,
            server_name,
            transport_params.clone(),
        )
        .map_err(QuicError::Tls)?;

        let mut conn = QuicConnection::new_client(socket, addr, local_cid, remote_cid, tls)?;
        conn.configure_client_resumption_state(
            cache_key,
            config.peer_transport_parameter_cache,
            config.token_cache,
        );

        // Drive the handshake
        conn.process_tls_output()?;
        conn.send_pending_frames().await?;

        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::RootCertStore;
    use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
    use starfish_reactor::reactor::Reactor;

    fn make_client_tls_config() -> ClientConfig {
        let _ = rustls::crypto::ring::default_provider().install_default();
        ClientConfig::builder()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth()
    }

    #[test]
    fn client_config_enable_early_data_sets_flag() {
        let config = QuicClientConfig::new(make_client_tls_config()).enable_early_data();

        assert!(config.tls_config.enable_early_data);
        assert_eq!(config.cid_len, 8);
    }

    #[test]
    fn connect_start_rejects_invalid_server_name() {
        let mut reactor = Reactor::new();

        let task = reactor.spawn_with_result(async move {
            let err = QuicClient::connect_start(
                "127.0.0.1:4433".parse().unwrap(),
                "bad name",
                QuicClientConfig::new(make_client_tls_config()),
            )
            .await
            .unwrap_err();

            assert!(matches!(err, QuicError::InvalidPacket(_)));
        });

        reactor.run();
        task.unwrap_result();
    }

    #[test]
    fn connect_start_uses_configured_cid_len() {
        let mut reactor = Reactor::new();

        let task = reactor.spawn_with_result(async move {
            let mut config = QuicClientConfig::new(make_client_tls_config());
            config.cid_len = 4;

            let conn =
                QuicClient::connect_start("127.0.0.1:4433".parse().unwrap(), "localhost", config)
                    .await
                    .unwrap();

            assert_eq!(conn.local_cid_bytes()[0].len(), 4);
        });

        reactor.run();
        task.unwrap_result();
    }
}
