//! QUIC stream — `AsyncRead + AsyncWrite` over a QUIC stream.
//!
//! [`QuicStream`] wraps a stream ID and a reference to the parent
//! [`QuicConnection`], providing the familiar async I/O interface.

use std::io;

use starfish_reactor::cooperative_io::async_read::AsyncRead;
use starfish_reactor::cooperative_io::async_write::AsyncWrite;
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;

use crate::connection::QuicConnection;
use crate::transport::stream_manager::StreamId;

/// A single QUIC stream providing `AsyncRead + AsyncWrite`.
///
/// Obtained by calling `QuicConnection::open_bidi_stream()` or by
/// accepting a remotely-initiated stream.
pub struct QuicStream<'a> {
    pub(crate) stream_id: StreamId,
    pub(crate) conn: &'a mut QuicConnection,
}

impl<'a> QuicStream<'a> {
    /// Create a new QuicStream handle.
    pub fn new(stream_id: StreamId, conn: &'a mut QuicConnection) -> Self {
        Self { stream_id, conn }
    }

    /// Get the stream ID.
    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }
}

impl<'a> AsyncRead for QuicStream<'a> {
    async fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<usize> {
        loop {
            // Try to read from the stream's receive buffer
            match self.conn.stream_recv(self.stream_id, buf) {
                Ok((n, _fin)) if n > 0 => return Ok(n),
                Ok((0, true)) => return Ok(0), // FIN, EOF
                Ok(_) => {
                    // No data available yet — poll the connection with timeout
                    self.conn
                        .poll_with_timeout(timeout)
                        .await
                        .map_err(|e| io::Error::other(e.to_string()))?;
                }
                Err(e) => {
                    return Err(io::Error::other(e.to_string()));
                }
            }
        }
    }
}

impl<'a> AsyncWrite for QuicStream<'a> {
    async fn write_with_timeout(
        &mut self,
        buf: &[u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        loop {
            let n = self
                .conn
                .stream_send(self.stream_id, buf, false)
                .map_err(|e| io::Error::other(e.to_string()))?;

            // Drive the connection so MAX_DATA / MAX_STREAM_DATA updates can arrive.
            self.conn
                .poll_with_timeout(timeout)
                .await
                .map_err(|e| io::Error::other(e.to_string()))?;

            if n > 0 {
                return Ok(n);
            }
        }
    }

    async fn flush_with_timeout(&mut self, timeout: &Option<IOTimeout>) -> io::Result<()> {
        self.conn
            .poll_with_timeout(timeout)
            .await
            .map_err(|e| io::Error::other(e.to_string()))
    }

    async fn close(&mut self) -> io::Result<()> {
        // Send FIN on this stream
        self.conn
            .stream_send(self.stream_id, &[], true)
            .map_err(|e| io::Error::other(e.to_string()))?;

        self.conn
            .poll()
            .await
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use starfish_core::preemptive_synchronization::future_extension::FutureExtension;

    use crate::connection::{
        ConnectionState, DEFAULT_IDLE_TIMEOUT_MS, DEFAULT_INITIAL_MAX_DATA,
        DEFAULT_INITIAL_MAX_STREAMS_BIDI, DEFAULT_INITIAL_MAX_STREAMS_UNI,
        DEFAULT_INITIAL_MAX_STREAM_DATA, MAX_LOCAL_UDP_PAYLOAD_SIZE,
    };
    use crate::crypto::keys::derive_initial_keys;
    use crate::packet;
    use crate::packet::header::PacketNumberSpace;
    use crate::packet::ConnectionId;
    use crate::transport::stream_manager::RecvState;

    fn make_connected_test_client() -> QuicConnection {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let std_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let socket = starfish_reactor::cooperative_io::udp_socket::UdpSocket::new(std_socket);
        let peer_addr = "127.0.0.1:4433".parse().unwrap();
        let local_cid = ConnectionId::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let remote_cid =
            ConnectionId::from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x11, 0x22]);

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

        let provider = rustls::crypto::ring::default_provider();
        let keys = derive_initial_keys(&[1, 2, 3, 4, 5, 6, 7, 8], true, &provider).unwrap();
        conn.keys
            .set_from_quic_keys(PacketNumberSpace::ApplicationData, keys);
        conn.state = ConnectionState::Connected;
        conn.endpoint_managed = true;
        conn.pending_frames.clear();
        conn
    }

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
            vec![
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::RSA_PSS_SHA256,
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
            ]
        }
    }

    #[test]
    fn read_with_timeout_consumes_buffered_data_and_fin() {
        let mut conn = make_connected_test_client();
        let stream_id = 1;
        conn.streams.accept_stream(stream_id).unwrap();
        let stream_state = conn.streams.get_mut(stream_id).unwrap();
        stream_state.recv_buf.insert(0, b"hello".to_vec());
        stream_state.final_size = Some(5);
        stream_state.recv = Some(RecvState::SizeKnown);

        let mut stream = QuicStream::new(stream_id, &mut conn);
        let mut buf = [0u8; 8];

        let n = stream
            .read_with_timeout(&mut buf, &None)
            .unwrap_result()
            .unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], b"hello");

        let eof = stream
            .read_with_timeout(&mut buf, &None)
            .unwrap_result()
            .unwrap();
        assert_eq!(eof, 0);
    }

    #[test]
    fn read_with_timeout_surfaces_reset_stream_errors() {
        let mut conn = make_connected_test_client();
        let stream_id = 1;
        conn.streams.accept_stream(stream_id).unwrap();
        let stream_state = conn.streams.get_mut(stream_id).unwrap();
        stream_state.recv = Some(RecvState::ResetRecvd);
        stream_state.reset_error_code = Some(42);

        let mut stream = QuicStream::new(stream_id, &mut conn);
        let mut buf = [0u8; 8];

        let err = stream
            .read_with_timeout(&mut buf, &None)
            .unwrap_result()
            .unwrap_err();
        assert!(err.to_string().contains("stream reset with code 42"));
    }

    #[test]
    fn write_with_timeout_sends_stream_data_frame() {
        let mut reactor = starfish_reactor::reactor::Reactor::new();
        let task = reactor.spawn_with_result(async move {
            let mut conn = make_connected_test_client();
            let stream_id = conn.open_bidi_stream().unwrap();

            let mut stream = QuicStream::new(stream_id, &mut conn);
            let written = stream
                .write_with_timeout(b"hello", &None)
                .await
                .map_err(|e| e.to_string())?;

            let state = conn.streams.get(stream_id).unwrap();
            if written != 5 || state.send_offset != 5 || state.fin_sent {
                return Err(format!(
                    "unexpected stream state: written={written} offset={} fin_sent={}",
                    state.send_offset, state.fin_sent
                ));
            }

            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }

    #[test]
    fn write_with_timeout_waits_when_flow_control_is_blocked() {
        let mut reactor = starfish_reactor::reactor::Reactor::new();
        let task = reactor.spawn_with_result(async move {
            let mut conn = make_connected_test_client();
            let stream_id = conn.open_bidi_stream().unwrap();
            conn.flow_control.set_send_max(0);
            conn.streams
                .get_mut(stream_id)
                .unwrap()
                .flow_control
                .set_send_max(0);

            let mut stream = QuicStream::new(stream_id, &mut conn);
            let timeout = Some(IOTimeout::from_duration(std::time::Duration::from_millis(
                1,
            )));
            match stream.write_with_timeout(b"hello", &timeout).await {
                Ok(n) => Err(format!(
                    "expected blocked write to wait or error, got Ok({n})"
                )),
                Err(_) => {
                    let state = conn.streams.get(stream_id).unwrap();
                    assert_eq!(state.send_offset, 0);
                    Ok::<(), String>(())
                }
            }
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }

    #[test]
    fn close_sends_fin_frame() {
        let mut reactor = starfish_reactor::reactor::Reactor::new();
        let task = reactor.spawn_with_result(async move {
            let mut conn = make_connected_test_client();
            let stream_id = conn.open_bidi_stream().unwrap();

            let mut stream = QuicStream::new(stream_id, &mut conn);
            stream.close().await.map_err(|e| e.to_string())?;

            let state = conn.streams.get(stream_id).unwrap();
            if !state.fin_sent || state.send_offset != 0 {
                return Err(format!(
                    "unexpected close state: offset={} fin_sent={}",
                    state.send_offset, state.fin_sent
                ));
            }

            Ok::<(), String>(())
        });

        reactor.run();
        task.unwrap_result().unwrap();
    }
}
