use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rcgen::{CertifiedKey, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_http::h3::client::H3Client;
use starfish_http::h3::server::H3Server;
use starfish_quic::{QuicClientConfig, QuicServerConfig};
use starfish_reactor::reactor::Reactor;

const AIOQUIC_SPEC: &str = "aioquic==1.2.0";

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

fn interop_script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("interop")
        .join("aioquic_h3_client.py")
}

fn interop_server_script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("interop")
        .join("aioquic_h3_server.py")
}

fn cached_aioquic_venv() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("aioquic-interop-venv")
}

fn venv_python(venv_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        venv_dir.join("Scripts").join("python.exe")
    } else {
        venv_dir.join("bin").join("python3")
    }
}

fn run_checked(mut command: Command, description: &str) -> io::Result<()> {
    let status = command.status()?;
    if status.success() {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "{description} failed with status {status}"
    )))
}

fn host_python() -> PathBuf {
    std::env::var_os("STARFISH_AIOQUIC_BOOTSTRAP_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("python3"))
}

fn ensure_aioquic_python() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("STARFISH_AIOQUIC_PYTHON") {
        return Ok(PathBuf::from(path));
    }

    let venv_dir = cached_aioquic_venv();
    let python = venv_python(&venv_dir);

    if python.exists() {
        let status = Command::new(&python)
            .args(["-c", "import aioquic"])
            .status()?;
        if status.success() {
            return Ok(python);
        }
    }

    if let Some(parent) = venv_dir.parent() {
        fs::create_dir_all(parent)?;
    }

    run_checked(
        {
            let mut cmd = Command::new(host_python());
            cmd.arg("-m").arg("venv").arg(&venv_dir);
            cmd
        },
        "create aioquic interop virtualenv",
    )?;

    run_checked(
        {
            let mut cmd = Command::new(&python);
            cmd.args(["-m", "pip", "install", "--upgrade", "pip"]);
            cmd
        },
        "upgrade pip in aioquic interop virtualenv",
    )?;

    run_checked(
        {
            let mut cmd = Command::new(&python);
            cmd.args(["-m", "pip", "install", AIOQUIC_SPEC]);
            cmd
        },
        "install aioquic",
    )?;

    Ok(python)
}

fn aioquic_available(python: &Path) -> io::Result<bool> {
    let status = Command::new(python)
        .args(["-c", "import aioquic"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.success())
}

fn default_aioquic_python() -> Option<PathBuf> {
    std::env::var_os("STARFISH_AIOQUIC_PYTHON")
        .map(PathBuf::from)
        .or_else(|| {
            let cached = venv_python(&cached_aioquic_venv());
            cached.exists().then_some(cached)
        })
}

fn aioquic_python_or_skip() -> Option<PathBuf> {
    if std::env::var_os("STARFISH_AIOQUIC_BOOTSTRAP").is_some() {
        return Some(ensure_aioquic_python().unwrap_or_else(|err| {
            panic!("failed to bootstrap aioquic interop environment: {err}")
        }));
    }

    let Some(python) = default_aioquic_python() else {
        eprintln!(
            "skipping aioquic HTTP/3 interop test: set STARFISH_AIOQUIC_PYTHON or STARFISH_AIOQUIC_BOOTSTRAP=1"
        );
        return None;
    };

    match aioquic_available(&python) {
        Ok(true) => Some(python),
        Ok(false) => {
            eprintln!(
                "skipping aioquic HTTP/3 interop test: {:?} does not provide aioquic; set STARFISH_AIOQUIC_BOOTSTRAP=1 to bootstrap",
                python
            );
            None
        }
        Err(err) => panic!("failed to validate aioquic Python at {:?}: {err}", python),
    }
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

struct ChildProcess {
    child: Option<Child>,
}

impl ChildProcess {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn wait_with_output(mut self) -> io::Result<Output> {
        self.child.take().unwrap().wait_with_output()
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
    let path = unique_temp_path("starfish-http3-ca", ".pem");
    fs::write(&path, certified.cert.pem())?;
    Ok(TempPath::new(path))
}

fn write_server_cert(certified: &CertifiedKey) -> io::Result<TempPath> {
    let path = unique_temp_path("starfish-http3-server-cert", ".pem");
    fs::write(&path, certified.cert.pem())?;
    Ok(TempPath::new(path))
}

fn write_server_key(certified: &CertifiedKey) -> io::Result<TempPath> {
    let path = unique_temp_path("starfish-http3-server-key", ".pem");
    fs::write(&path, certified.key_pair.serialize_pem())?;
    Ok(TempPath::new(path))
}

fn wait_for_file(path: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("timed out waiting for {}", path.display()),
    ))
}

fn make_client_config(certified: &CertifiedKey) -> QuicClientConfig {
    let mut roots = RootCertStore::empty();
    roots.add(certified.cert.der().clone()).unwrap();
    let client_tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    QuicClientConfig::new(client_tls)
}

#[test]
fn h3_server_interops_with_aioquic_client() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let Some(python) = aioquic_python_or_skip() else {
        return;
    };

    let certified = generate_self_signed_cert().unwrap();
    let ca_cert = write_ca_cert(&certified).unwrap();
    let server_config = make_server_config(&certified);
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
            let Some((stream_id, request)) = conn
                .recv_request()
                .await
                .map_err(|e| format!("recv_request failed: {e}"))?
            else {
                return Err("server did not receive request".into());
            };

            if request.uri().path() != "/interop" {
                return Err(format!("unexpected request path: {}", request.uri().path()));
            }
            if request
                .headers()
                .get("user-agent")
                .and_then(|value| value.to_str().ok())
                != Some("starfish-aioquic-interop")
            {
                return Err("missing aioquic user-agent header".into());
            }

            let response = http::Response::builder()
                .status(http::StatusCode::OK)
                .header("content-type", "text/plain")
                .body(b"interop-ok".to_vec())
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
        Ok(Err(err)) => panic!("server failed before interop client started: {err}"),
        Err(err) => panic!("timed out waiting for H3 server readiness: {err}"),
    }

    let authority = format!("localhost:{}", addr.port());
    let output = Command::new(&python)
        .arg(interop_script_path())
        .args(["--host", "127.0.0.1"])
        .args(["--port", &addr.port().to_string()])
        .args(["--server-name", "localhost"])
        .args(["--authority", &authority])
        .args(["--path", "/interop"])
        .args(["--ca-cert", ca_cert.as_path().to_str().unwrap()])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let server_result = result_rx.recv_timeout(Duration::from_secs(2));

    assert!(
        output.status.success(),
        "aioquic interop client failed with status {}.\nstdout:\n{}\nstderr:\n{}\nserver result: {:?}",
        output.status,
        stdout,
        stderr,
        server_result,
    );
    assert!(
        stdout.contains("status: 200"),
        "interop client did not report HTTP 200.\nstdout:\n{}\nstderr:\n{}\nserver result: {:?}",
        stdout,
        stderr,
        server_result,
    );
    assert!(
        stdout.contains("body: interop-ok"),
        "interop client did not report the expected body.\nstdout:\n{}\nstderr:\n{}\nserver result: {:?}",
        stdout,
        stderr,
        server_result,
    );

    match server_result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("H3 server task failed during aioquic interop: {err}"),
        Err(err) => panic!("timed out waiting for H3 server result: {err}"),
    }
}

#[test]
fn h3_client_interops_with_aioquic_server() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let Some(python) = aioquic_python_or_skip() else {
        return;
    };

    let certified = generate_self_signed_cert().unwrap();
    let server_cert = write_server_cert(&certified).unwrap();
    let server_key = write_server_key(&certified).unwrap();
    let client_config = make_client_config(&certified);
    let addr = next_addr();
    let ready_file = TempPath::new(unique_temp_path("starfish-http3-aioquic-ready", ".txt"));

    let server = ChildProcess::new(
        Command::new(&python)
            .arg(interop_server_script_path())
            .args(["--host", "127.0.0.1"])
            .args(["--port", &addr.port().to_string()])
            .args(["--cert", server_cert.as_path().to_str().unwrap()])
            .args(["--key", server_key.as_path().to_str().unwrap()])
            .args(["--ready-file", ready_file.as_path().to_str().unwrap()])
            .args(["--response-body", "aioquic-ok"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );

    wait_for_file(ready_file.as_path(), Duration::from_secs(10)).unwrap();

    let mut reactor = Reactor::new();
    let client = reactor.spawn_with_result(async move {
        let mut client = H3Client::connect(addr, "localhost", client_config)
            .await
            .map_err(|e| e.to_string())?;
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!("https://localhost:{}/interop", addr.port()))
            .header("user-agent", "starfish-h3-interop")
            .header("content-type", "text/plain")
            .body(b"ping-from-starfish".to_vec())
            .unwrap();

        let response = client.send(request).await.map_err(|e| e.to_string())?;
        if response.status() != http::StatusCode::OK {
            return Err(format!("unexpected response status: {}", response.status()));
        }
        if response.body().as_slice() != b"aioquic-ok" {
            return Err(format!("unexpected response body: {:?}", response.body()));
        }
        if response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            != Some("text/plain")
        {
            return Err("unexpected response content-type".into());
        }

        Ok::<(), String>(())
    });

    reactor.run();

    let client_result = client.unwrap_result();
    let output = server.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "aioquic interop server failed with status {}.\nstdout:\n{}\nstderr:\n{}\nclient result: {:?}",
        output.status,
        stdout,
        stderr,
        client_result,
    );
    assert!(
        stdout.contains("method: POST"),
        "interop server did not observe the expected method.\nstdout:\n{}\nstderr:\n{}\nclient result: {:?}",
        stdout,
        stderr,
        client_result,
    );
    assert!(
        stdout.contains("path: /interop"),
        "interop server did not observe the expected path.\nstdout:\n{}\nstderr:\n{}\nclient result: {:?}",
        stdout,
        stderr,
        client_result,
    );
    assert!(
        stdout.contains("user-agent: starfish-h3-interop"),
        "interop server did not observe the expected user-agent.\nstdout:\n{}\nstderr:\n{}\nclient result: {:?}",
        stdout,
        stderr,
        client_result,
    );
    assert!(
        stdout.contains("body: ping-from-starfish"),
        "interop server did not observe the expected request body.\nstdout:\n{}\nstderr:\n{}\nclient result: {:?}",
        stdout,
        stderr,
        client_result,
    );

    match client_result {
        Ok(()) => {}
        Err(err) => panic!("H3 client failed during aioquic interop: {err}"),
    }
}
