use rcgen::{CertifiedKey, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_quic::{
    ConnectionState, QuicClient, QuicClientConfig, QuicConnection, QuicEndpoint, QuicError,
    QuicListener, QuicServerConfig,
};
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;
use starfish_reactor::cooperative_synchronization::delayed_future::cooperative_sleep;
use starfish_reactor::reactor::Reactor;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

fn next_addr() -> SocketAddr {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    addr
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

fn make_configs() -> (QuicServerConfig, QuicClientConfig) {
    let certified = generate_self_signed_cert().unwrap();
    let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));

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

fn malformed_initial_packet() -> Vec<u8> {
    vec![
        0xc0, 0x00, 0x00, 0x00, 0x01, 4, 0x10, 0x11, 0x12, 0x13, 4, 0x20, 0x21, 0x22, 0x23,
    ]
}

async fn recv_all(conn: &mut QuicConnection, sid: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf = [0u8; 256];

    loop {
        match conn.stream_recv(sid, &mut buf) {
            Ok((n, fin)) => {
                if n > 0 {
                    out.extend_from_slice(&buf[..n]);
                }
                if fin {
                    return Ok(out);
                }
                conn.poll().await.map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

async fn poll_ignoring_timeouts(
    conn: &mut QuicConnection,
    timeout: Duration,
) -> Result<(), String> {
    let timeout = Some(IOTimeout::from_duration(timeout));
    match conn.poll_with_timeout(&timeout).await {
        Ok(()) => Ok(()),
        Err(QuicError::Io(err))
            if matches!(err.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
        {
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

async fn wait_for_tickets(conn: &mut QuicConnection, min_tickets: u32) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while conn.tls13_tickets_received() < min_tickets {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "timed out waiting for {min_tickets} TLS 1.3 ticket(s); got {}",
                conn.tls13_tickets_received()
            ));
        }
        poll_ignoring_timeouts(conn, remaining.min(Duration::from_millis(50))).await?;
    }
    Ok(())
}

#[test]
fn listener_roundtrip_stream_data() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();

    let server = reactor.spawn_with_result(async move {
        let mut listener = QuicListener::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let mut conn = listener.accept().await.map_err(|e| e.to_string())?;

        let sid = loop {
            if let Some(sid) = conn.accept_incoming_bidi() {
                break sid;
            }
            conn.poll().await.map_err(|e| e.to_string())?;
        };

        let request = recv_all(&mut conn, sid).await?;
        if request != b"ping" {
            return Err(format!("unexpected request payload: {request:?}"));
        }

        conn.stream_send(sid, b"pong", true)
            .map_err(|e| e.to_string())?;
        conn.flush().await.map_err(|e| e.to_string())?;
        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut conn = QuicClient::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;
        let sid = conn.open_bidi_stream().map_err(|e| e.to_string())?;
        conn.stream_send(sid, b"ping", true)
            .map_err(|e| e.to_string())?;
        conn.flush().await.map_err(|e| e.to_string())?;

        let response = recv_all(&mut conn, sid).await?;
        if response != b"pong" {
            return Err(format!("unexpected response payload: {response:?}"));
        }
        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn listener_ignores_bad_initial_and_accepts_next_client() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();

    let server = reactor.spawn_with_result(async move {
        let mut listener = QuicListener::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let conn = listener.accept().await.map_err(|e| e.to_string())?;
        Ok::<bool, String>(conn.is_established())
    });

    let client = reactor.spawn_with_result(async move {
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        sender
            .send_to(&malformed_initial_packet(), addr)
            .map_err(|e| e.to_string())?;
        cooperative_sleep(Duration::from_millis(10)).await;

        let conn = QuicClient::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<bool, String>(conn.is_established())
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn endpoint_routes_connection_and_stream_data() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();

    let server = reactor.spawn_with_result(async move {
        let mut endpoint = QuicEndpoint::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let handle = endpoint.accept().await.map_err(|e| e.to_string())?;

        let sid = loop {
            if let Some(sid) = endpoint.accept_incoming_bidi(handle) {
                break sid;
            }
            endpoint.drive().await.map_err(|e| e.to_string())?;
        };

        let mut request = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match endpoint.stream_recv(handle, sid, &mut buf) {
                Ok((n, fin)) => {
                    if n > 0 {
                        request.extend_from_slice(&buf[..n]);
                    }
                    if fin {
                        break;
                    }
                    endpoint.drive().await.map_err(|e| e.to_string())?;
                }
                Err(e) => return Err(e.to_string()),
            }
        }

        if request != b"ping" {
            return Err(format!("unexpected endpoint request payload: {request:?}"));
        }

        endpoint
            .stream_send(handle, sid, b"pong", true)
            .map_err(|e| e.to_string())?;
        endpoint.flush_all().await;
        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut conn = QuicClient::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;
        let sid = conn.open_bidi_stream().map_err(|e| e.to_string())?;
        conn.stream_send(sid, b"ping", true)
            .map_err(|e| e.to_string())?;
        conn.flush().await.map_err(|e| e.to_string())?;

        let response = recv_all(&mut conn, sid).await?;
        if response != b"pong" {
            return Err(format!(
                "unexpected endpoint response payload: {response:?}"
            ));
        }
        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn client_initiated_key_update_keeps_stream_traffic_flowing() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();

    let server = reactor.spawn_with_result(async move {
        let mut listener = QuicListener::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let mut conn = listener.accept().await.map_err(|e| e.to_string())?;

        let first_sid = loop {
            if let Some(sid) = conn.accept_incoming_bidi() {
                break sid;
            }
            conn.poll().await.map_err(|e| e.to_string())?;
        };

        let first = recv_all(&mut conn, first_sid).await?;
        if first != b"phase-one" {
            return Err(format!("unexpected first payload: {first:?}"));
        }

        conn.stream_send(first_sid, b"phase-two", true)
            .map_err(|e| e.to_string())?;
        conn.flush().await.map_err(|e| e.to_string())?;

        let second_sid = loop {
            if let Some(sid) = conn.accept_incoming_bidi() {
                break sid;
            }
            conn.poll().await.map_err(|e| e.to_string())?;
        };

        let second = recv_all(&mut conn, second_sid).await?;
        if second != b"phase-three" {
            return Err(format!("unexpected second payload: {second:?}"));
        }

        conn.stream_send(second_sid, b"phase-four", true)
            .map_err(|e| e.to_string())?;
        conn.flush().await.map_err(|e| e.to_string())?;

        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut conn = QuicClient::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;

        if !conn.initiate_key_update().map_err(|e| e.to_string())? {
            return Err("client failed to initiate key update".into());
        }

        let first_sid = conn.open_bidi_stream().map_err(|e| e.to_string())?;
        conn.stream_send(first_sid, b"phase-one", true)
            .map_err(|e| e.to_string())?;
        conn.flush().await.map_err(|e| e.to_string())?;

        let first_response = recv_all(&mut conn, first_sid).await?;
        if first_response != b"phase-two" {
            return Err(format!("unexpected first response: {first_response:?}"));
        }

        let second_sid = conn.open_bidi_stream().map_err(|e| e.to_string())?;
        conn.stream_send(second_sid, b"phase-three", true)
            .map_err(|e| e.to_string())?;
        conn.flush().await.map_err(|e| e.to_string())?;

        let second_response = recv_all(&mut conn, second_sid).await?;
        if second_response != b"phase-four" {
            return Err(format!("unexpected second response: {second_response:?}"));
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn resumed_connection_sends_and_accepts_zero_rtt_stream_data() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();
    let server_config = server_config.enable_early_data();
    let client_config = client_config.enable_early_data();
    let ticket_ready = Arc::new(AtomicBool::new(false));

    let server_ticket_ready = Arc::clone(&ticket_ready);
    let server = reactor.spawn_with_result(async move {
        let mut listener = QuicListener::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let mut first_conn = listener.accept().await.map_err(|e| e.to_string())?;

        let first_sid = loop {
            if let Some(sid) = first_conn.accept_incoming_bidi() {
                break sid;
            }
            first_conn.poll().await.map_err(|e| e.to_string())?;
        };

        let first = recv_all(&mut first_conn, first_sid).await?;
        if first != b"ticket-please" {
            return Err(format!("unexpected first payload: {first:?}"));
        }

        first_conn
            .stream_send(first_sid, b"ticket-issued", true)
            .map_err(|e| e.to_string())?;
        first_conn.flush().await.map_err(|e| e.to_string())?;

        let ticket_deadline = Instant::now() + Duration::from_secs(2);
        while !server_ticket_ready.load(Ordering::SeqCst) {
            let remaining = ticket_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for client to receive resumption ticket".into());
            }
            poll_ignoring_timeouts(&mut first_conn, remaining.min(Duration::from_millis(50)))
                .await?;
        }

        drop(first_conn);

        let mut second_conn = listener.accept().await.map_err(|e| e.to_string())?;
        let second_sid = loop {
            if let Some(sid) = second_conn.accept_incoming_bidi() {
                break sid;
            }
            second_conn.poll().await.map_err(|e| e.to_string())?;
        };

        let second = recv_all(&mut second_conn, second_sid).await?;
        if second != b"early-ping" {
            return Err(format!("unexpected second payload: {second:?}"));
        }

        second_conn
            .stream_send(second_sid, b"early-pong", true)
            .map_err(|e| e.to_string())?;
        second_conn.flush().await.map_err(|e| e.to_string())?;

        Ok::<bool, String>(true)
    });

    let client_ticket_ready = Arc::clone(&ticket_ready);
    let client = reactor.spawn_with_result(async move {
        let mut first_conn = QuicClient::connect(addr, "localhost", client_config.clone())
            .await
            .map_err(|e| e.to_string())?;
        let first_sid = first_conn.open_bidi_stream().map_err(|e| e.to_string())?;
        first_conn
            .stream_send(first_sid, b"ticket-please", true)
            .map_err(|e| e.to_string())?;
        first_conn.flush().await.map_err(|e| e.to_string())?;

        let first_response = recv_all(&mut first_conn, first_sid).await?;
        if first_response != b"ticket-issued" {
            return Err(format!("unexpected first response: {first_response:?}"));
        }

        wait_for_tickets(&mut first_conn, 1).await?;
        client_ticket_ready.store(true, Ordering::SeqCst);
        drop(first_conn);

        let mut resumed = QuicClient::connect_start(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;
        if resumed.is_established() {
            return Err("resumed connection completed before early data send".into());
        }
        if !resumed.can_send_early_data() {
            return Err("resumed connection did not arm 0-RTT send keys".into());
        }

        let early_sid = resumed.open_bidi_stream().map_err(|e| e.to_string())?;
        resumed
            .stream_send(early_sid, b"early-ping", true)
            .map_err(|e| e.to_string())?;
        resumed.flush().await.map_err(|e| e.to_string())?;

        let response = recv_all(&mut resumed, early_sid).await?;
        if response != b"early-pong" {
            return Err(format!("unexpected early response: {response:?}"));
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while resumed.early_data_accepted().is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for early-data acceptance state".into());
            }
            poll_ignoring_timeouts(&mut resumed, remaining.min(Duration::from_millis(50))).await?;
        }

        if resumed.early_data_accepted() != Some(true) {
            return Err(format!(
                "expected accepted early data, got {:?}",
                resumed.early_data_accepted()
            ));
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn graceful_close_moves_peer_to_draining() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();

    let server = reactor.spawn_with_result(async move {
        let mut listener = QuicListener::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let mut conn = listener.accept().await.map_err(|e| e.to_string())?;
        conn.close(0x44, b"server shutdown")
            .await
            .map_err(|e| e.to_string())?;
        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut conn = QuicClient::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;

        let deadline = Instant::now() + Duration::from_secs(2);
        while conn.state() != ConnectionState::Draining {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out waiting for peer close; state={:?}",
                    conn.state()
                ));
            }
            poll_ignoring_timeouts(&mut conn, remaining.min(Duration::from_millis(50))).await?;
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn multiple_bidirectional_streams_roundtrip() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();

    let server = reactor.spawn_with_result(async move {
        let mut listener = QuicListener::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let mut conn = listener.accept().await.map_err(|e| e.to_string())?;
        let mut handled = 0usize;

        while handled < 3 {
            if let Some(sid) = conn.accept_incoming_bidi() {
                let request = recv_all(&mut conn, sid).await?;
                let mut response = b"reply:".to_vec();
                response.extend_from_slice(&request);
                conn.stream_send(sid, &response, true)
                    .map_err(|e| e.to_string())?;
                handled += 1;
                continue;
            }
            conn.poll().await.map_err(|e| e.to_string())?;
        }

        conn.flush().await.map_err(|e| e.to_string())?;
        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut conn = QuicClient::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;

        let payloads = [
            b"alpha".as_slice(),
            b"bravo".as_slice(),
            b"charlie".as_slice(),
        ];
        let mut stream_ids = Vec::new();
        for payload in payloads {
            let sid = conn.open_bidi_stream().map_err(|e| e.to_string())?;
            conn.stream_send(sid, payload, true)
                .map_err(|e| e.to_string())?;
            stream_ids.push((sid, payload.to_vec()));
        }
        conn.flush().await.map_err(|e| e.to_string())?;

        for (sid, payload) in stream_ids {
            let response = recv_all(&mut conn, sid).await?;
            let mut expected = b"reply:".to_vec();
            expected.extend_from_slice(&payload);
            if response != expected {
                return Err(format!("unexpected multi-stream response: {response:?}"));
            }
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn server_initiated_uni_stream_delivers_large_payload() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();
    let payload = vec![0x5au8; 32 * 1024];

    let server_payload = payload.clone();
    let server = reactor.spawn_with_result(async move {
        let mut listener = QuicListener::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let mut conn = listener.accept().await.map_err(|e| e.to_string())?;
        let sid = conn.open_uni_stream().map_err(|e| e.to_string())?;
        for chunk in server_payload.chunks(1024) {
            conn.stream_send(sid, chunk, false)
                .map_err(|e| e.to_string())?;
            conn.flush().await.map_err(|e| e.to_string())?;
            poll_ignoring_timeouts(&mut conn, Duration::from_millis(10)).await?;
        }
        conn.stream_send(sid, &[], true)
            .map_err(|e| e.to_string())?;
        conn.flush().await.map_err(|e| e.to_string())?;
        poll_ignoring_timeouts(&mut conn, Duration::from_millis(500)).await?;
        Ok::<bool, String>(true)
    });

    let client_payload = payload.clone();
    let client = reactor.spawn_with_result(async move {
        let mut conn = QuicClient::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;

        let sid = loop {
            if let Some(sid) = conn.accept_incoming_uni() {
                break sid;
            }
            conn.poll().await.map_err(|e| e.to_string())?;
        };

        let received = recv_all(&mut conn, sid).await?;
        if received != client_payload {
            return Err(format!(
                "unexpected large unidirectional payload length: {}",
                received.len()
            ));
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}
