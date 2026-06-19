use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rcgen::{CertifiedKey, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_http::h3::client::H3Client;
use starfish_http::h3::server::H3Server;
use starfish_quic::{QuicClientConfig, QuicServerConfig};
use starfish_reactor::reactor::Reactor;

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

fn make_server_config(certified: &CertifiedKey) -> QuicServerConfig {
    let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));

    let mut server_tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key_der)
        .unwrap();
    server_tls.alpn_protocols = vec![b"h3".to_vec()];

    QuicServerConfig::new(server_tls)
}

fn make_client_config(certified: &CertifiedKey) -> QuicClientConfig {
    let mut roots = RootCertStore::empty();
    roots.add(certified.cert.der().clone()).unwrap();
    let client_tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    QuicClientConfig::new(client_tls)
}

fn unique_temp_path(prefix: &str, suffix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}{suffix}", std::process::id()))
}

struct TempPath(PathBuf);

impl TempPath {
    fn new(path: PathBuf) -> Self {
        Self(path)
    }

    fn as_path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(path: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn as_path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ChildProcess {
    child: Option<Child>,
}

impl ChildProcess {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn terminate_with_output(mut self) -> io::Result<Output> {
        let mut child = self.child.take().unwrap();
        let _ = child.kill();
        child.wait_with_output()
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn write_ca_cert(certified: &CertifiedKey) -> io::Result<TempPath> {
    let path = unique_temp_path("starfish-http3-quiche-ca", ".pem");
    fs::write(&path, certified.cert.pem())?;
    Ok(TempPath::new(path))
}

fn write_server_cert(certified: &CertifiedKey) -> io::Result<TempPath> {
    let path = unique_temp_path("starfish-http3-quiche-server-cert", ".pem");
    fs::write(&path, certified.cert.pem())?;
    Ok(TempPath::new(path))
}

fn write_server_key(certified: &CertifiedKey) -> io::Result<TempPath> {
    let path = unique_temp_path("starfish-http3-quiche-server-key", ".pem");
    fs::write(&path, certified.key_pair.serialize_pem())?;
    Ok(TempPath::new(path))
}

fn write_request_body(contents: &[u8]) -> io::Result<TempPath> {
    let path = unique_temp_path("starfish-http3-quiche-request", ".bin");
    fs::write(&path, contents)?;
    Ok(TempPath::new(path))
}

fn wait_for_udp_port_to_bind(addr: SocketAddr, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match std::net::UdpSocket::bind(addr) {
            Ok(socket) => {
                drop(socket);
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return Ok(()),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("timed out waiting for UDP listener on {addr}"),
    ))
}

fn quiche_client() -> PathBuf {
    std::env::var_os("STARFISH_QUICHE_CLIENT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("quiche-client"))
}

fn quiche_server() -> PathBuf {
    std::env::var_os("STARFISH_QUICHE_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("quiche-server"))
}

fn command_available(command: &Path) -> bool {
    match Command::new(command)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(_) => true,
        Err(err) if err.kind() == io::ErrorKind::NotFound => false,
        Err(err) => panic!("failed to probe {:?}: {err}", command),
    }
}

fn quiche_client_or_skip() -> Option<PathBuf> {
    let client = quiche_client();
    if command_available(&client) {
        Some(client)
    } else {
        eprintln!(
            "skipping quiche HTTP/3 interop test: set STARFISH_QUICHE_CLIENT to a valid quiche-client binary"
        );
        None
    }
}

fn quiche_server_or_skip() -> Option<PathBuf> {
    let server = quiche_server();
    if command_available(&server) {
        Some(server)
    } else {
        eprintln!(
            "skipping quiche HTTP/3 interop test: set STARFISH_QUICHE_SERVER to a valid quiche-server binary"
        );
        None
    }
}

fn body_dump_path(dump_dir: &Path, file_name: &str) -> PathBuf {
    dump_dir.join(file_name.trim_start_matches('/'))
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

#[test]
fn h3_server_interops_with_quiche_client() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let Some(quiche_client) = quiche_client_or_skip() else {
        return;
    };

    let certified = generate_self_signed_cert().unwrap();
    let server_config = make_server_config(&certified);
    let ca_cert = write_ca_cert(&certified).unwrap();
    let request_body = write_request_body(b"ping-from-quiche").unwrap();
    let addr = next_addr();
    let response_dump_dir =
        TempDir::new(unique_temp_path("starfish-http3-quiche-client-dump", "")).unwrap();

    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let (result_tx, result_rx) = mpsc::channel::<Result<(), String>>();

    thread::spawn(move || {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut reactor = Reactor::new();
        let server = reactor.spawn_with_result(async move {
            let mut server = match H3Server::bind(addr, server_config).await {
                Ok(server) => {
                    let _ = ready_tx.send(Ok(()));
                    server
                }
                Err(err) => {
                    let message = err.to_string();
                    let _ = ready_tx.send(Err(message.clone()));
                    return Err(message);
                }
            };

            let mut conn = server
                .accept()
                .await
                .map_err(|e| format!("accept failed: {e}"))?;
            let Some((stream_id, request)) = conn
                .recv_request()
                .await
                .map_err(|e| format!("recv_request failed: {e}"))?
            else {
                return Err("server did not receive request".into());
            };

            if request.method() != http::Method::POST {
                return Err(format!("unexpected request method: {}", request.method()));
            }
            if request.uri().path() != "/interop" {
                return Err(format!("unexpected request path: {}", request.uri().path()));
            }
            if request
                .headers()
                .get("user-agent")
                .and_then(|value| value.to_str().ok())
                != Some("quiche")
            {
                return Err("missing quiche user-agent header".into());
            }
            if request.body().as_slice() != b"ping-from-quiche" {
                return Err(format!("unexpected request body: {:?}", request.body()));
            }

            let response = http::Response::builder()
                .status(http::StatusCode::OK)
                .header("content-type", "text/plain")
                .body(b"starfish-quiche-client-ok".to_vec())
                .unwrap();
            conn.send_response(stream_id, response)
                .await
                .map_err(|e| format!("send_response failed: {e}"))?;

            Ok::<(), String>(())
        });

        reactor.run();
        let _ = result_tx.send(server.unwrap_result());
    });

    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("server failed before quiche client started: {err}"),
        Err(err) => panic!("timed out waiting for H3 server readiness: {err}"),
    }

    let url = format!("https://localhost:{}/interop", addr.port());
    let output = Command::new(&quiche_client)
        .args(["--method", "POST"])
        .args(["--body", request_body.as_path().to_str().unwrap()])
        .args(["--wire-version", "1"])
        .args(["--connect-to", &format!("127.0.0.1:{}", addr.port())])
        .args(["--trust-origin-ca-pem", ca_cert.as_path().to_str().unwrap()])
        .args([
            "--dump-responses",
            response_dump_dir.as_path().to_str().unwrap(),
        ])
        .arg(&url)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let server_result = result_rx.recv_timeout(Duration::from_secs(2));

    assert!(
        output.status.success(),
        "quiche client failed with status {}.\nstdout:\n{}\nstderr:\n{}\nserver result: {:?}",
        output.status,
        stdout,
        stderr,
        server_result,
    );
    let body = fs::read(body_dump_path(response_dump_dir.as_path(), "interop")).unwrap();
    assert_eq!(body, b"starfish-quiche-client-ok");

    match server_result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("H3 server task failed during quiche interop: {err}"),
        Err(err) => panic!("timed out waiting for H3 server result: {err}"),
    }
}

#[test]
fn h3_server_handles_quiche_request_burst() {
    const REQUESTS: usize = 16;

    let _ = rustls::crypto::ring::default_provider().install_default();
    let Some(quiche_client) = quiche_client_or_skip() else {
        return;
    };

    let certified = generate_self_signed_cert().unwrap();
    let server_config = make_server_config(&certified);
    let ca_cert = write_ca_cert(&certified).unwrap();
    let addr = next_addr();

    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let (result_tx, result_rx) = mpsc::channel::<Result<(), String>>();

    thread::spawn(move || {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut reactor = Reactor::new();
        let server = reactor.spawn_with_result(async move {
            let mut server = match H3Server::bind(addr, server_config).await {
                Ok(server) => {
                    let _ = ready_tx.send(Ok(()));
                    server
                }
                Err(err) => {
                    let message = err.to_string();
                    let _ = ready_tx.send(Err(message.clone()));
                    return Err(message);
                }
            };

            let mut conn = server
                .accept()
                .await
                .map_err(|e| format!("accept failed: {e}"))?;

            for request_index in 0..REQUESTS {
                let Some((stream_id, request)) = conn
                    .recv_request()
                    .await
                    .map_err(|e| format!("recv_request failed: {e}"))?
                else {
                    return Err(format!(
                        "server stopped receiving requests after {} of {}",
                        request_index, REQUESTS
                    ));
                };

                if request.method() != http::Method::GET {
                    return Err(format!("unexpected request method: {}", request.method()));
                }
                if request.uri().path() != "/burst" {
                    return Err(format!("unexpected request path: {}", request.uri().path()));
                }

                let response = http::Response::builder()
                    .status(http::StatusCode::OK)
                    .header("content-type", "text/plain")
                    .body(b"burst-ok".to_vec())
                    .unwrap();
                conn.send_response(stream_id, response)
                    .await
                    .map_err(|e| format!("send_response failed: {e}"))?;
            }

            Ok::<(), String>(())
        });

        reactor.run();
        let _ = result_tx.send(server.unwrap_result());
    });

    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("server failed before quiche burst client started: {err}"),
        Err(err) => panic!("timed out waiting for H3 server readiness: {err}"),
    }

    let url = format!("https://localhost:{}/burst", addr.port());
    let output = Command::new(&quiche_client)
        .args(["--wire-version", "1"])
        .args(["--requests", &REQUESTS.to_string()])
        .args(["--connect-to", &format!("127.0.0.1:{}", addr.port())])
        .args(["--trust-origin-ca-pem", ca_cert.as_path().to_str().unwrap()])
        .args(["--dump-json"])
        .arg(&url)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let server_result = result_rx.recv_timeout(Duration::from_secs(2));

    assert!(
        output.status.success(),
        "quiche burst client failed with status {}.\nstdout:\n{}\nstderr:\n{}\nserver result: {:?}",
        output.status,
        stdout,
        stderr,
        server_result,
    );
    assert_eq!(
        count_occurrences(&stdout, "\"value\": \"200\""),
        REQUESTS,
        "quiche burst client did not observe {} successful responses.\nstdout:\n{}\nstderr:\n{}\nserver result: {:?}",
        REQUESTS,
        stdout,
        stderr,
        server_result,
    );

    match server_result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("H3 server task failed during quiche burst interop: {err}"),
        Err(err) => panic!("timed out waiting for H3 server result: {err}"),
    }
}

#[test]
fn h3_client_interops_with_quiche_server() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let Some(quiche_server) = quiche_server_or_skip() else {
        return;
    };

    let certified = generate_self_signed_cert().unwrap();
    let client_config = make_client_config(&certified);
    let server_cert = write_server_cert(&certified).unwrap();
    let server_key = write_server_key(&certified).unwrap();
    let root_dir = TempDir::new(unique_temp_path("starfish-http3-quiche-root", "")).unwrap();
    fs::write(
        root_dir.as_path().join("interop"),
        b"quiche-server-ok".as_slice(),
    )
    .unwrap();

    let addr = next_addr();
    let server = ChildProcess::new(
        Command::new(&quiche_server)
            .args(["--listen", &addr.to_string()])
            .args(["--cert", server_cert.as_path().to_str().unwrap()])
            .args(["--key", server_key.as_path().to_str().unwrap()])
            .args(["--root", root_dir.as_path().to_str().unwrap()])
            .args(["--name", "localhost"])
            .args(["--no-retry"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );

    wait_for_udp_port_to_bind(addr, Duration::from_secs(10)).unwrap();

    let mut reactor = Reactor::new();
    let client = reactor.spawn_with_result(async move {
        let mut client = H3Client::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri(format!("https://localhost:{}/interop", addr.port()))
            .header("user-agent", "starfish-h3-quiche")
            .body(Vec::new())
            .unwrap();

        let response = client.send(request).await.map_err(|e| e.to_string())?;
        if response.status() != http::StatusCode::OK {
            return Err(format!("unexpected response status: {}", response.status()));
        }
        if response.body().as_slice() != b"quiche-server-ok" {
            return Err(format!("unexpected response body: {:?}", response.body()));
        }
        if response
            .headers()
            .get("server")
            .and_then(|value| value.to_str().ok())
            != Some("quiche")
        {
            return Err("unexpected response server header".into());
        }

        Ok::<(), String>(())
    });

    reactor.run();

    let client_result = client.unwrap_result();
    let output = server.terminate_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    match client_result {
        Ok(()) => {}
        Err(err) => panic!(
            "H3 client failed during quiche interop: {err}\nquiche server stdout:\n{}\nquiche server stderr:\n{}",
            stdout, stderr
        ),
    }
}
