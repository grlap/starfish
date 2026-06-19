use rcgen::{CertifiedKey, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_http::h1::server::H1Server;
use starfish_http::{HttpClient, HttpServer, ProxyConfig};
use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_reactor::cooperative_io::tcp_listener::TcpListener;
use starfish_reactor::reactor::Reactor;
use starfish_tls::tls_server::TlsServer;
use std::net::SocketAddr;

fn next_addr() -> SocketAddr {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
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

fn make_tls_configs() -> (ServerConfig, ClientConfig) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certified = generate_self_signed_cert().unwrap();
    let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));

    let server_tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key_der)
        .unwrap();

    let mut roots = RootCertStore::empty();
    roots.add(certified.cert.der().clone()).unwrap();
    let mut client_tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client_tls.alpn_protocols = vec![b"http/1.1".to_vec()];

    (server_tls, client_tls)
}

#[test]
fn http_client_supports_https_roundtrip() {
    let mut reactor = Reactor::new();
    let addr = next_addr();
    let (server_tls, client_tls) = make_tls_configs();

    let server = reactor.spawn_with_result(async move {
        let mut listener = TcpListener::bind(addr).await.map_err(|e| e.to_string())?;
        let stream = listener.accept().await.map_err(|e| e.to_string())?;
        let tls_stream = TlsServer::new(stream, server_tls)
            .await
            .map_err(|e| e.to_string())?;
        let mut server = H1Server::new(tls_stream);

        let Some(request) = server.recv_request().await.map_err(|e| e.to_string())? else {
            return Err("missing HTTPS request".into());
        };
        if request.uri().path() != "/secure" {
            return Err(format!("unexpected HTTPS path: {}", request.uri()));
        }

        server
            .send_response(
                http::Response::builder()
                    .status(http::StatusCode::OK)
                    .body(b"tls-ok".to_vec())
                    .unwrap(),
            )
            .await
            .map_err(|e| e.to_string())?;

        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut client = HttpClient::new();
        client.set_tls_config(client_tls);

        let response = client
            .send(
                http::Request::builder()
                    .method(http::Method::GET)
                    .uri(format!("https://localhost:{}/secure", addr.port()))
                    .header(http::header::HOST, format!("localhost:{}", addr.port()))
                    .body(Vec::new())
                    .unwrap(),
            )
            .await
            .map_err(|e| e.to_string())?;

        if response.status() != http::StatusCode::OK || response.body().as_slice() != b"tls-ok" {
            return Err(format!("unexpected HTTPS response: {:?}", response));
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn http_client_reuses_set_cookie_on_follow_up_request() {
    let mut reactor = Reactor::new();
    let addr = next_addr();

    let server = reactor.spawn_with_result(async move {
        let mut server = HttpServer::bind(addr).await.map_err(|e| e.to_string())?;
        let mut conn = server.accept().await.map_err(|e| e.to_string())?;

        let Some(first_request) = conn.recv_request().await.map_err(|e| e.to_string())? else {
            return Err("missing first request".into());
        };
        if first_request.headers().get(http::header::COOKIE).is_some() {
            return Err("unexpected cookie on first request".into());
        }

        conn.send_response(
            http::Response::builder()
                .status(http::StatusCode::OK)
                .header(http::header::SET_COOKIE, "session=abc; Path=/")
                .body(b"first".to_vec())
                .unwrap(),
        )
        .await
        .map_err(|e| e.to_string())?;

        let Some(second_request) = conn.recv_request().await.map_err(|e| e.to_string())? else {
            return Err("missing second request".into());
        };
        if second_request
            .headers()
            .get(http::header::COOKIE)
            .and_then(|value| value.to_str().ok())
            != Some("session=abc")
        {
            return Err(format!(
                "missing cookie on second request: {:?}",
                second_request.headers()
            ));
        }

        conn.send_response(
            http::Response::builder()
                .status(http::StatusCode::OK)
                .body(b"second".to_vec())
                .unwrap(),
        )
        .await
        .map_err(|e| e.to_string())?;

        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut client = HttpClient::new();

        let first = client
            .send(
                http::Request::builder()
                    .method(http::Method::GET)
                    .uri(format!("http://{addr}/one"))
                    .header(http::header::HOST, addr.to_string())
                    .body(Vec::new())
                    .unwrap(),
            )
            .await
            .map_err(|e| e.to_string())?;
        if first.body().as_slice() != b"first" {
            return Err(format!("unexpected first response: {:?}", first));
        }

        let second = client
            .send(
                http::Request::builder()
                    .method(http::Method::GET)
                    .uri(format!("http://{addr}/two"))
                    .header(http::header::HOST, addr.to_string())
                    .body(Vec::new())
                    .unwrap(),
            )
            .await
            .map_err(|e| e.to_string())?;
        if second.body().as_slice() != b"second" {
            return Err(format!("unexpected second response: {:?}", second));
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn http_client_supports_http_proxy_requests() {
    let mut reactor = Reactor::new();
    let proxy_addr = next_addr();

    let proxy = reactor.spawn_with_result(async move {
        let mut listener = TcpListener::bind(proxy_addr)
            .await
            .map_err(|e| e.to_string())?;
        let stream = listener.accept().await.map_err(|e| e.to_string())?;
        let mut proxy = H1Server::new(stream);

        let Some(request) = proxy.recv_request().await.map_err(|e| e.to_string())? else {
            return Err("missing proxy request".into());
        };
        if request.uri() != &http::Uri::from_static("http://example.test/proxied?x=1") {
            return Err(format!("unexpected proxy URI: {}", request.uri()));
        }

        proxy
            .send_response(
                http::Response::builder()
                    .status(http::StatusCode::OK)
                    .body(b"proxied".to_vec())
                    .unwrap(),
            )
            .await
            .map_err(|e| e.to_string())?;

        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut client = HttpClient::new();
        client.set_proxy(Some(
            ProxyConfig::parse(&format!("http://127.0.0.1:{}", proxy_addr.port()))
                .map_err(|e| e.to_string())?,
        ));

        let response = client
            .send(
                http::Request::builder()
                    .method(http::Method::GET)
                    .uri("http://example.test/proxied?x=1")
                    .header(http::header::HOST, "example.test")
                    .body(Vec::new())
                    .unwrap(),
            )
            .await
            .map_err(|e| e.to_string())?;

        if response.body().as_slice() != b"proxied" {
            return Err(format!("unexpected proxy response: {:?}", response));
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(proxy.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}

#[test]
fn http_client_supports_socks5_proxy_requests() {
    let mut reactor = Reactor::new();
    let proxy_addr = next_addr();

    let proxy = reactor.spawn_with_result(async move {
        let mut listener = TcpListener::bind(proxy_addr)
            .await
            .map_err(|e| e.to_string())?;
        let mut stream = listener.accept().await.map_err(|e| e.to_string())?;

        let mut greeting = [0u8; 3];
        stream
            .read_exact(&mut greeting)
            .await
            .map_err(|e| e.to_string())?;
        if greeting != [0x05, 0x01, 0x00] {
            return Err(format!("unexpected SOCKS greeting: {greeting:?}"));
        }
        stream
            .write_all(&[0x05, 0x00])
            .await
            .map_err(|e| e.to_string())?;
        stream.flush().await.map_err(|e| e.to_string())?;

        let mut request_head = [0u8; 4];
        stream
            .read_exact(&mut request_head)
            .await
            .map_err(|e| e.to_string())?;
        if request_head[..3] != [0x05, 0x01, 0x00] {
            return Err(format!("unexpected SOCKS request head: {request_head:?}"));
        }

        let target_host = match request_head[3] {
            0x03 => {
                let mut len = [0u8; 1];
                stream
                    .read_exact(&mut len)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut host = vec![0u8; len[0] as usize];
                stream
                    .read_exact(&mut host)
                    .await
                    .map_err(|e| e.to_string())?;
                String::from_utf8(host).map_err(|e| e.to_string())?
            }
            other => return Err(format!("unexpected SOCKS address type: {other:#x}")),
        };
        if target_host != "example.test" {
            return Err(format!("unexpected SOCKS target host: {target_host}"));
        }

        let mut port = [0u8; 2];
        stream
            .read_exact(&mut port)
            .await
            .map_err(|e| e.to_string())?;
        if u16::from_be_bytes(port) != 80 {
            return Err(format!(
                "unexpected SOCKS target port: {}",
                u16::from_be_bytes(port)
            ));
        }

        stream
            .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
            .await
            .map_err(|e| e.to_string())?;
        stream.flush().await.map_err(|e| e.to_string())?;

        let mut server = H1Server::new(stream);
        let Some(request) = server.recv_request().await.map_err(|e| e.to_string())? else {
            return Err("missing tunneled HTTP request".into());
        };
        if request.uri().path() != "/socks" {
            return Err(format!("unexpected tunneled path: {}", request.uri()));
        }

        server
            .send_response(
                http::Response::builder()
                    .status(http::StatusCode::OK)
                    .body(b"socks-ok".to_vec())
                    .unwrap(),
            )
            .await
            .map_err(|e| e.to_string())?;

        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut client = HttpClient::new();
        client.set_proxy(Some(
            ProxyConfig::parse(&format!("socks5://127.0.0.1:{}", proxy_addr.port()))
                .map_err(|e| e.to_string())?,
        ));

        let response = client
            .send(
                http::Request::builder()
                    .method(http::Method::GET)
                    .uri("http://example.test/socks")
                    .header(http::header::HOST, "example.test")
                    .body(Vec::new())
                    .unwrap(),
            )
            .await
            .map_err(|e| e.to_string())?;

        if response.body().as_slice() != b"socks-ok" {
            return Err(format!("unexpected SOCKS response: {:?}", response));
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(proxy.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}
