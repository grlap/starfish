use rcgen::{CertifiedKey, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_http::h3::client::H3Client;
use starfish_http::h3::server::H3Server;
use starfish_quic::{QuicClientConfig, QuicServerConfig};
use starfish_reactor::reactor::Reactor;
use std::net::SocketAddr;

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

#[test]
fn h3_roundtrip_request_response() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();

    let server = reactor.spawn_with_result(async move {
        let mut server = H3Server::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let mut conn = server.accept().await.map_err(|e| e.to_string())?;

        let Some((stream_id, request)) = conn.recv_request().await.map_err(|e| e.to_string())?
        else {
            return Err("server did not receive request".into());
        };

        if request.uri().path() != "/hello" {
            return Err(format!("unexpected request path: {}", request.uri().path()));
        }

        let response = http::Response::builder()
            .status(http::StatusCode::OK)
            .header("content-type", "text/plain")
            .body(b"world".to_vec())
            .unwrap();
        conn.send_response(stream_id, response)
            .await
            .map_err(|e| e.to_string())?;

        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut client = H3Client::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri("https://localhost/hello")
            .header("user-agent", "starfish-http-test")
            .body(Vec::new())
            .unwrap();

        let response = client.send(request).await.map_err(|e| e.to_string())?;
        if response.status() != http::StatusCode::OK {
            return Err(format!("unexpected response status: {}", response.status()));
        }
        if response.body().as_slice() != b"world" {
            return Err(format!("unexpected response body: {:?}", response.body()));
        }
        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn h3_client_can_pipeline_multiple_requests() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();

    let server = reactor.spawn_with_result(async move {
        let mut server = H3Server::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let mut conn = server.accept().await.map_err(|e| e.to_string())?;

        let Some((first_stream, first_request)) =
            conn.recv_request().await.map_err(|e| e.to_string())?
        else {
            return Err("server did not receive first request".into());
        };
        let Some((second_stream, second_request)) =
            conn.recv_request().await.map_err(|e| e.to_string())?
        else {
            return Err("server did not receive second request".into());
        };

        if first_request.uri().path() != "/one" {
            return Err(format!(
                "unexpected first request path: {}",
                first_request.uri().path()
            ));
        }
        if second_request.uri().path() != "/two" {
            return Err(format!(
                "unexpected second request path: {}",
                second_request.uri().path()
            ));
        }

        let second_response = http::Response::builder()
            .status(http::StatusCode::OK)
            .header("x-path", "/two")
            .body(b"second".to_vec())
            .unwrap();
        conn.send_response(second_stream, second_response)
            .await
            .map_err(|e| e.to_string())?;

        let first_response = http::Response::builder()
            .status(http::StatusCode::OK)
            .header("x-path", "/one")
            .body(b"first".to_vec())
            .unwrap();
        conn.send_response(first_stream, first_response)
            .await
            .map_err(|e| e.to_string())?;

        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut client = H3Client::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;

        let first_request = http::Request::builder()
            .method(http::Method::GET)
            .uri("https://localhost/one")
            .body(Vec::new())
            .unwrap();
        let second_request = http::Request::builder()
            .method(http::Method::GET)
            .uri("https://localhost/two")
            .body(Vec::new())
            .unwrap();

        let first_stream = client
            .send_request(first_request)
            .await
            .map_err(|e| e.to_string())?;
        let second_stream = client
            .send_request(second_request)
            .await
            .map_err(|e| e.to_string())?;

        let second_response = client
            .recv_response(second_stream)
            .await
            .map_err(|e| e.to_string())?;
        let first_response = client
            .recv_response(first_stream)
            .await
            .map_err(|e| e.to_string())?;

        if second_response
            .headers()
            .get("x-path")
            .and_then(|value| value.to_str().ok())
            != Some("/two")
        {
            return Err("unexpected second response headers".into());
        }
        if second_response.body().as_slice() != b"second" {
            return Err(format!(
                "unexpected second response body: {:?}",
                second_response.body()
            ));
        }
        if first_response
            .headers()
            .get("x-path")
            .and_then(|value| value.to_str().ok())
            != Some("/one")
        {
            return Err("unexpected first response headers".into());
        }
        if first_response.body().as_slice() != b"first" {
            return Err(format!(
                "unexpected first response body: {:?}",
                first_response.body()
            ));
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn h3_client_receives_server_push() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_config, client_config) = make_configs();

    let server = reactor.spawn_with_result(async move {
        let mut server = H3Server::bind(addr, server_config)
            .await
            .map_err(|e| e.to_string())?;
        let mut conn = server.accept().await.map_err(|e| e.to_string())?;

        let Some((stream_id, request)) = conn.recv_request().await.map_err(|e| e.to_string())?
        else {
            return Err("server did not receive request".into());
        };
        if request.uri().path() != "/" {
            return Err(format!("unexpected request path: {}", request.uri().path()));
        }

        conn.send_push(
            stream_id,
            http::Request::builder()
                .method(http::Method::GET)
                .uri("https://localhost/style.css")
                .body(Vec::new())
                .unwrap(),
            http::Response::builder()
                .status(http::StatusCode::OK)
                .header("content-type", "text/css")
                .body(b"body { color: black; }".to_vec())
                .unwrap(),
        )
        .await
        .map_err(|e| e.to_string())?;

        conn.send_response(
            stream_id,
            http::Response::builder()
                .status(http::StatusCode::OK)
                .header("content-type", "text/html")
                .body(b"<html></html>".to_vec())
                .unwrap(),
        )
        .await
        .map_err(|e| e.to_string())?;

        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut client = H3Client::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;
        let response = client
            .send(
                http::Request::builder()
                    .method(http::Method::GET)
                    .uri("https://localhost/")
                    .body(Vec::new())
                    .unwrap(),
            )
            .await
            .map_err(|e| e.to_string())?;
        if response.body().as_slice() != b"<html></html>" {
            return Err(format!("unexpected response body: {:?}", response.body()));
        }

        let pushed = client.recv_push().await.map_err(|e| e.to_string())?;
        if pushed.push_id != 0
            || pushed
                .request_headers
                .iter()
                .find(|(name, _)| name == ":path")
                .map(|(_, value)| value.as_str())
                != Some("/style.css")
            || pushed.body.as_slice() != b"body { color: black; }"
        {
            return Err(format!("unexpected pushed response: {:?}", pushed));
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}
