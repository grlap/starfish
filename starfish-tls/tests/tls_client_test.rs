use std::{
    error, io,
    net::ToSocketAddrs,
    str,
    sync::atomic::{AtomicU16, Ordering},
    sync::{Arc, Once},
};

use rcgen::{CertifiedKey, KeyPair};
use rustls::pki_types::{pem::PemObject, PrivateKeyDer};
use rustls::ServerConfig;

use starfish_reactor::cooperative_io::async_read::AsyncRead;
use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;
use starfish_reactor::cooperative_io::async_write::AsyncWrite;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_reactor::cooperative_io::tcp_listener::TcpListener;
use starfish_reactor::cooperative_io::tcp_stream::TcpStream;
use starfish_reactor::reactor::Reactor;

use starfish_core::preemptive_synchronization::future_extension::FutureExtension;

use rustls::RootCertStore;
use starfish_tls::accept_any_server_cert_verifier::AcceptAnyServerCertVerifier;
use starfish_tls::error::TlsError;
use starfish_tls::tls_client::TlsClient;
use starfish_tls::tls_server::TlsServer;

static RUSTLS_PROVIDER: Once = Once::new();

fn install_rustls_provider() {
    RUSTLS_PROVIDER.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

fn create_client_config() -> rustls::ClientConfig {
    install_rustls_provider();

    // Load root certificates.
    //
    let root_store = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    config
        .dangerous()
        .set_certificate_verifier(Arc::new(AcceptAnyServerCertVerifier));

    config
}

static PORT_COUNTER: AtomicU16 = AtomicU16::new(19100);

fn next_addr() -> String {
    let port = PORT_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("127.0.0.1:{}", port)
}

fn create_server_config() -> ServerConfig {
    install_rustls_provider();

    let certified_key = generate_self_signed_cert().unwrap();
    let key_der = PrivateKeyDer::from_pem(
        rustls::pki_types::pem::SectionKind::PrivateKey,
        certified_key.key_pair.serialize_der(),
    )
    .unwrap();
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certified_key.cert.der().to_owned()], key_der)
        .unwrap()
}

fn generate_self_signed_cert() -> Result<CertifiedKey, rcgen::Error> {
    let subject_alt_names = vec!["localhost".to_string()];

    let mut params = rcgen::CertificateParams::new(subject_alt_names)?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");

    let key_pair = KeyPair::generate()?;
    let cert: rcgen::Certificate = params.self_signed(&key_pair)?;

    Ok(CertifiedKey { cert, key_pair })
}

pub async fn tls_http_perform_get_request<S>(
    stream: S,
    server: &str,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>>
where
    S: AsyncRead + AsyncWrite,
{
    let config = create_client_config();

    // Establish TLS connection
    let mut tcp_client = TlsClient::new(stream, config, server).await?;
    // Prepare HTTP request

    let request = format!(
        "GET {} HTTP/1.1\r\n\
         Host: {}\r\n\
         Connection: close\r\n\
         \r\n",
        path, server
    );

    println!("[] HttpRequest: {}", request);

    // Send request.
    //
    tcp_client.write_all(request.as_bytes()).await?;
    tcp_client.flush().await?;

    println!("Request written_all");

    // Read response.
    //
    let mut response_buf = Vec::new();
    loop {
        let mut buf = [0u8; 16384];
        let count = tcp_client.read(&mut buf).await.unwrap();
        if count == 0 {
            break;
        }
        response_buf.extend_from_slice(&buf[..count]);
    }

    // Close connection.
    //
    tcp_client.close().await?;

    // Convert response to string
    let response_str = String::from_utf8_lossy(&response_buf).to_string();

    Ok(response_str)
}

async fn tls_get_request(
    url: &str,
    server_name: &str,
    path: &str,
) -> Result<String, Box<dyn error::Error>> {
    let socket_address = url
        .to_socket_addrs()?
        .next()
        .ok_or(io::Error::other("failed to resolve address"))?;

    println!("socket_address: {} {:?}", url, socket_address);

    let tcp_stream = TcpStream::connect(socket_address).await?;

    tls_http_perform_get_request(tcp_stream, server_name, path).await
}

async fn test_ssl_tcp_server(address: String) -> bool {
    install_rustls_provider();

    let certified_key = generate_self_signed_cert().unwrap();

    let key_der = PrivateKeyDer::from_pem(
        rustls::pki_types::pem::SectionKind::PrivateKey,
        certified_key.key_pair.serialize_der(),
    )
    .unwrap();

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certified_key.cert.der().to_owned()], key_der)
        .unwrap();

    let mut tcp_listener = TcpListener::bind(address.clone()).await.unwrap();

    let tcp_stream = tcp_listener.accept().await.unwrap();

    let mut tls_server = TlsServer::new(tcp_stream, config).await.unwrap();

    let mut buffer: Vec<u8> = vec![1; 1024];

    let read_bytes = tls_server
        .read(buffer.as_mut_slice())
        .await
        .expect("tcp_read failed");

    let body = str::from_utf8(&buffer[..read_bytes]).unwrap();

    println!("body: {}", body);

    let reply_string = "reply string\n\n";

    let written_bytes = tls_server
        .write(reply_string.as_bytes().to_vec().as_mut_slice())
        .await
        .expect("tcp_write failed");

    println!("written: {}:", written_bytes);

    tls_server.close().await.unwrap();

    true
}

#[test]
fn test_ssl_read() {
    let addr = next_addr();
    let mut reactor = Reactor::new();

    let server_addr = addr.clone();
    let result_server_wait = reactor.spawn_with_result(test_ssl_tcp_server(server_addr));

    let client_addr = addr.clone();
    let result_client_wait = reactor.spawn_with_result(async move {
        let response = tls_get_request(&client_addr, "localhost", "/").await?;
        Ok::<bool, Box<dyn error::Error>>(response.contains("reply string"))
    });

    reactor.run();

    assert!(result_server_wait.unwrap_result());
    assert!(result_client_wait.unwrap_result().unwrap());
}

#[test]
fn test_ssl_server() {
    let addr = next_addr();
    let mut reactor = Reactor::new();

    let server_addr = addr.clone();
    let result_server_wait = reactor.spawn_with_result(test_ssl_tcp_server(server_addr));

    let result_client_wait = reactor.spawn_with_result(async move {
        let response = tls_get_request(&addr, "localhost", "/").await?;
        Ok::<bool, Box<dyn error::Error>>(response.contains("reply string"))
    });

    reactor.run();

    assert!(result_server_wait.unwrap_result());
    assert!(result_client_wait.unwrap_result().unwrap());
}

// --- New tests ---

/// Test multiple read/write round-trips on a single TLS connection.
#[test]
fn test_multiple_round_trips() {
    let addr = next_addr();
    let mut reactor = Reactor::new();

    let server_addr = addr.clone();
    let _server = reactor.spawn_with_result(async move {
        let config = create_server_config();
        let mut listener = TcpListener::bind(server_addr).await.unwrap();
        let stream = listener.accept().await.unwrap();
        let mut tls = TlsServer::new(stream, config).await.unwrap();

        for i in 0u32..5 {
            let mut buf = [0u8; 256];
            let n = tls.read(&mut buf).await.unwrap();
            let msg = str::from_utf8(&buf[..n]).unwrap();
            assert_eq!(msg, format!("ping {}", i));

            let reply = format!("pong {}", i);
            tls.write_all(reply.as_bytes()).await.unwrap();
            tls.flush().await.unwrap();
        }

        tls.close().await.unwrap();
        true
    });

    let client_addr = addr.clone();
    let _client = reactor.spawn_with_result(async move {
        let config = create_client_config();
        let stream = TcpStream::connect(client_addr.parse().unwrap())
            .await
            .unwrap();
        let mut tls = TlsClient::new(stream, config, "localhost").await.unwrap();

        for i in 0u32..5 {
            let msg = format!("ping {}", i);
            tls.write_all(msg.as_bytes()).await.unwrap();
            tls.flush().await.unwrap();

            let mut buf = [0u8; 256];
            let n = tls.read(&mut buf).await.unwrap();
            let reply = str::from_utf8(&buf[..n]).unwrap();
            assert_eq!(reply, format!("pong {}", i));
        }

        tls.close().await.unwrap();
        true
    });

    reactor.run();

    assert!(_server.unwrap_result());
    assert!(_client.unwrap_result());
}

/// Test transferring a large payload (128 KB) over TLS.
#[test]
fn test_large_payload() {
    let addr = next_addr();
    let mut reactor = Reactor::new();

    let payload_size: usize = 128 * 1024;

    let server_addr = addr.clone();
    let _server = reactor.spawn_with_result(async move {
        let config = create_server_config();
        let mut listener = TcpListener::bind(server_addr).await.unwrap();
        let stream = listener.accept().await.unwrap();
        let mut tls = TlsServer::new(stream, config).await.unwrap();

        // Read until we get the full payload.
        let mut received = Vec::new();
        loop {
            let mut buf = [0u8; 8192];
            let n = tls.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
            if received.len() >= payload_size {
                break;
            }
        }

        assert_eq!(received.len(), payload_size);
        // Verify content pattern.
        for (i, byte) in received.iter().enumerate() {
            assert_eq!(*byte, (i % 251) as u8);
        }

        // Echo back the length as confirmation.
        let reply = format!("{}", received.len());
        tls.write_all(reply.as_bytes()).await.unwrap();
        tls.close().await.unwrap();
        true
    });

    let client_addr = addr.clone();
    let _client = reactor.spawn_with_result(async move {
        let config = create_client_config();
        let stream = TcpStream::connect(client_addr.parse().unwrap())
            .await
            .unwrap();
        let mut tls = TlsClient::new(stream, config, "localhost").await.unwrap();

        // Build a deterministic payload.
        let mut payload = vec![0u8; payload_size];
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }

        tls.write_all(&payload).await.unwrap();
        tls.flush().await.unwrap();

        // Read confirmation.
        let mut buf = [0u8; 64];
        let n = tls.read(&mut buf).await.unwrap();
        let reply = str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(reply, format!("{}", payload_size));

        tls.close().await.unwrap();
        true
    });

    reactor.run();

    assert!(_server.unwrap_result());
    assert!(_client.unwrap_result());
}

/// Test reading with a very small buffer to exercise partial-read paths.
#[test]
fn test_small_buffer_reads() {
    let addr = next_addr();
    let mut reactor = Reactor::new();

    let server_addr = addr.clone();
    let _server = reactor.spawn_with_result(async move {
        let config = create_server_config();
        let mut listener = TcpListener::bind(server_addr).await.unwrap();
        let stream = listener.accept().await.unwrap();
        let mut tls = TlsServer::new(stream, config).await.unwrap();

        let message = "Hello, this is a test message for small buffer reads!";
        tls.write_all(message.as_bytes()).await.unwrap();
        tls.close().await.unwrap();
        true
    });

    let client_addr = addr.clone();
    let _client = reactor.spawn_with_result(async move {
        let config = create_client_config();
        let stream = TcpStream::connect(client_addr.parse().unwrap())
            .await
            .unwrap();
        let mut tls = TlsClient::new(stream, config, "localhost").await.unwrap();

        // Read with a tiny 4-byte buffer.
        let mut received = Vec::new();
        loop {
            let mut buf = [0u8; 4];
            let n = tls.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
        }

        let result = String::from_utf8(received).unwrap();
        assert_eq!(
            result,
            "Hello, this is a test message for small buffer reads!"
        );
        true
    });

    reactor.run();

    assert!(_server.unwrap_result());
    assert!(_client.unwrap_result());
}

/// Test server writing first before client sends anything.
#[test]
fn test_server_writes_first() {
    let addr = next_addr();
    let mut reactor = Reactor::new();

    let server_addr = addr.clone();
    let _server = reactor.spawn_with_result(async move {
        let config = create_server_config();
        let mut listener = TcpListener::bind(server_addr).await.unwrap();
        let stream = listener.accept().await.unwrap();
        let mut tls = TlsServer::new(stream, config).await.unwrap();

        // Server sends first without waiting for client data.
        let greeting = "welcome";
        tls.write_all(greeting.as_bytes()).await.unwrap();
        tls.flush().await.unwrap();

        // Then read the client's response.
        let mut buf = [0u8; 256];
        let n = tls.read(&mut buf).await.unwrap();
        let msg = str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(msg, "thanks");

        tls.close().await.unwrap();
        true
    });

    let client_addr = addr.clone();
    let _client = reactor.spawn_with_result(async move {
        let config = create_client_config();
        let stream = TcpStream::connect(client_addr.parse().unwrap())
            .await
            .unwrap();
        let mut tls = TlsClient::new(stream, config, "localhost").await.unwrap();

        // Client reads first — server sends greeting.
        let mut buf = [0u8; 256];
        let n = tls.read(&mut buf).await.unwrap();
        let greeting = str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(greeting, "welcome");

        // Reply.
        tls.write_all(b"thanks").await.unwrap();
        tls.close().await.unwrap();
        true
    });

    reactor.run();

    assert!(_server.unwrap_result());
    assert!(_client.unwrap_result());
}

/// Test multiple concurrent TLS connections on the same reactor.
#[test]
fn test_concurrent_connections() {
    let addr1 = next_addr();
    let addr2 = next_addr();
    let mut reactor = Reactor::new();

    // Server 1
    let s1_addr = addr1.clone();
    let _s1 = reactor.spawn_with_result(async move {
        let config = create_server_config();
        let mut listener = TcpListener::bind(s1_addr).await.unwrap();
        let stream = listener.accept().await.unwrap();
        let mut tls = TlsServer::new(stream, config).await.unwrap();

        let mut buf = [0u8; 256];
        let n = tls.read(&mut buf).await.unwrap();
        assert_eq!(str::from_utf8(&buf[..n]).unwrap(), "conn1");
        tls.write_all(b"ack1").await.unwrap();
        tls.close().await.unwrap();
        true
    });

    // Server 2
    let s2_addr = addr2.clone();
    let _s2 = reactor.spawn_with_result(async move {
        let config = create_server_config();
        let mut listener = TcpListener::bind(s2_addr).await.unwrap();
        let stream = listener.accept().await.unwrap();
        let mut tls = TlsServer::new(stream, config).await.unwrap();

        let mut buf = [0u8; 256];
        let n = tls.read(&mut buf).await.unwrap();
        assert_eq!(str::from_utf8(&buf[..n]).unwrap(), "conn2");
        tls.write_all(b"ack2").await.unwrap();
        tls.close().await.unwrap();
        true
    });

    // Client 1
    let c1_addr = addr1.clone();
    let _c1 = reactor.spawn_with_result(async move {
        let config = create_client_config();
        let stream = TcpStream::connect(c1_addr.parse().unwrap()).await.unwrap();
        let mut tls = TlsClient::new(stream, config, "localhost").await.unwrap();

        tls.write_all(b"conn1").await.unwrap();
        tls.flush().await.unwrap();
        let mut buf = [0u8; 256];
        let n = tls.read(&mut buf).await.unwrap();
        assert_eq!(str::from_utf8(&buf[..n]).unwrap(), "ack1");
        tls.close().await.unwrap();
        true
    });

    // Client 2
    let c2_addr = addr2.clone();
    let _c2 = reactor.spawn_with_result(async move {
        let config = create_client_config();
        let stream = TcpStream::connect(c2_addr.parse().unwrap()).await.unwrap();
        let mut tls = TlsClient::new(stream, config, "localhost").await.unwrap();

        tls.write_all(b"conn2").await.unwrap();
        tls.flush().await.unwrap();
        let mut buf = [0u8; 256];
        let n = tls.read(&mut buf).await.unwrap();
        assert_eq!(str::from_utf8(&buf[..n]).unwrap(), "ack2");
        tls.close().await.unwrap();
        true
    });

    reactor.run();

    assert!(_s1.unwrap_result());
    assert!(_s2.unwrap_result());
    assert!(_c1.unwrap_result());
    assert!(_c2.unwrap_result());
}

/// Test that close sends TLS close_notify and both sides shut down cleanly.
#[test]
fn test_clean_shutdown() {
    let addr = next_addr();
    let mut reactor = Reactor::new();

    let server_addr = addr.clone();
    let _server = reactor.spawn_with_result(async move {
        let config = create_server_config();
        let mut listener = TcpListener::bind(server_addr).await.unwrap();
        let stream = listener.accept().await.unwrap();
        let mut tls = TlsServer::new(stream, config).await.unwrap();

        // Write and immediately close.
        tls.write_all(b"goodbye").await.unwrap();
        tls.close().await.unwrap();
        true
    });

    let client_addr = addr.clone();
    let _client = reactor.spawn_with_result(async move {
        let config = create_client_config();
        let stream = TcpStream::connect(client_addr.parse().unwrap())
            .await
            .unwrap();
        let mut tls = TlsClient::new(stream, config, "localhost").await.unwrap();

        // Read the message.
        let mut buf = [0u8; 256];
        let n = tls.read(&mut buf).await.unwrap();
        assert_eq!(str::from_utf8(&buf[..n]).unwrap(), "goodbye");

        // Next read should return 0 (EOF from close_notify).
        let n = tls.read(&mut buf).await.unwrap();
        assert_eq!(n, 0);

        tls.close().await.unwrap();
        true
    });

    reactor.run();

    assert!(_server.unwrap_result());
    assert!(_client.unwrap_result());
}

/// Test transferring a 1 MB payload in both directions over TLS.
#[test]
fn test_1mb_payload() {
    let addr = next_addr();
    let mut reactor = Reactor::new();

    let payload_size: usize = 1024 * 1024; // 1 MB

    let server_addr = addr.clone();
    let _server = reactor.spawn_with_result(async move {
        let config = create_server_config();
        let mut listener = TcpListener::bind(server_addr).await.unwrap();
        let stream = listener.accept().await.unwrap();
        let mut tls = TlsServer::new(stream, config).await.unwrap();

        // Read the full payload.
        let mut received = Vec::new();
        loop {
            let mut buf = [0u8; 16384];
            let n = tls.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
            if received.len() >= payload_size {
                break;
            }
        }

        assert_eq!(received.len(), payload_size);

        // Verify content.
        for (i, byte) in received.iter().enumerate() {
            assert_eq!(*byte, (i % 251) as u8, "mismatch at byte {}", i);
        }

        // Echo back the payload (server -> client large transfer).
        tls.write_all(&received).await.unwrap();
        tls.close().await.unwrap();
        true
    });

    let client_addr = addr.clone();
    let _client = reactor.spawn_with_result(async move {
        let config = create_client_config();
        let stream = TcpStream::connect(client_addr.parse().unwrap())
            .await
            .unwrap();
        let mut tls = TlsClient::new(stream, config, "localhost").await.unwrap();

        // Build deterministic payload.
        let mut payload = vec![0u8; payload_size];
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }

        // Send 1 MB to server.
        tls.write_all(&payload).await.unwrap();
        tls.flush().await.unwrap();

        // Read 1 MB echo back from server.
        let mut received = Vec::new();
        loop {
            let mut buf = [0u8; 16384];
            let n = tls.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
            if received.len() >= payload_size {
                break;
            }
        }

        assert_eq!(received.len(), payload_size);
        assert_eq!(received, payload, "echoed payload does not match");

        tls.close().await.unwrap();
        true
    });

    reactor.run();

    assert!(_server.unwrap_result());
    assert!(_client.unwrap_result());
}

#[test]
fn test_invalid_server_name_returns_typed_error() {
    let mut reactor = Reactor::new();

    let addr = next_addr();

    let server_addr = addr.clone();
    let _server = reactor.spawn(async move {
        let mut listener = TcpListener::bind(&server_addr).await.unwrap();
        let _stream = listener.accept().await.unwrap();
        // Client will fail before handshake, just accept and drop.
    });

    let client_addr = addr;
    let result = reactor.spawn_with_result(async move {
        let config = create_client_config();
        let stream = TcpStream::connect(client_addr.parse().unwrap())
            .await
            .unwrap();

        // "not a valid name!!!" is not a valid DNS name — fails at ServerName::try_from,
        // before any I/O, and produces TlsError::InvalidServerName.
        match TlsClient::new(stream, config, "not a valid name!!!").await {
            Err(TlsError::InvalidServerName(_)) => true,
            Err(other) => panic!("expected InvalidServerName, got: {other}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    });

    reactor.run();
    assert!(result.unwrap_result());
}

#[test]
fn test_connection_metadata_after_handshake() {
    let mut reactor = Reactor::new();

    let addr = next_addr();

    let server_addr = addr.clone();
    let _server = reactor.spawn(async move {
        let config = create_server_config();
        let mut listener = TcpListener::bind(&server_addr).await.unwrap();
        let stream = listener.accept().await.unwrap();
        let server = TlsServer::new(stream, config).await.unwrap();

        // Server should also see the negotiated metadata.
        assert!(
            server.protocol_version().is_some(),
            "server should have protocol version"
        );
        assert!(
            server.negotiated_cipher_suite().is_some(),
            "server should have cipher suite"
        );

        // No client certificate was presented (no mutual TLS).
        assert!(server.peer_certificates().is_none());
    });

    let client_addr = addr;
    let result = reactor.spawn_with_result(async move {
        let config = create_client_config();
        let stream = TcpStream::connect(client_addr.parse().unwrap())
            .await
            .unwrap();
        let client = TlsClient::new(stream, config, "localhost").await.unwrap();

        // After handshake, protocol version and cipher suite must be available.
        let version = client
            .protocol_version()
            .expect("protocol_version should be Some after handshake");
        let cipher = client
            .negotiated_cipher_suite()
            .expect("negotiated_cipher_suite should be Some after handshake");

        // Verify they are reasonable values.
        println!("negotiated: {:?}, cipher: {:?}", version, cipher);

        // ALPN is None because we didn't configure it — that's expected.
        assert!(client.alpn_protocol().is_none());

        true
    });

    reactor.run();
    assert!(result.unwrap_result());
}
