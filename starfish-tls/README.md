# starfish-tls

Async TLS/SSL support for the Starfish reactor using rustls.

## Overview

`starfish-tls` provides TLS client and server wrappers that integrate with the `starfish-reactor` async I/O system. It uses rustls (a pure-Rust TLS implementation) for cryptographic operations, requiring no OpenSSL dependency.

## Features

- **Async TLS client** - Wrap any async stream with TLS encryption
- **Async TLS server** - Accept TLS connections with custom certificates
- **Timeout support** - All operations support optional timeouts
- **Pure Rust** - Uses rustls with AWS-LC-RS cryptography
- **Stream-agnostic** - Works with any type implementing AsyncRead + AsyncWrite

## Quick Start

### TLS Client

```rust
use starfish_reactor::cooperative_io::tcp_stream::TcpStream;
use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_tls::tls_client::TlsClient;
use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;

// Create TLS config with root certificates
let root_store = RootCertStore {
    roots: webpki_roots::TLS_SERVER_ROOTS.into(),
};
let config = ClientConfig::builder()
    .with_root_certificates(root_store)
    .with_no_client_auth();

// Connect TCP stream
let tcp_stream = TcpStream::connect("example.com:443".parse().unwrap()).await?;

// Wrap with TLS (handshake happens automatically)
let mut tls_client = TlsClient::new(tcp_stream, config, "example.com").await?;

// Send HTTP request
tls_client.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n").await?;
tls_client.flush().await?;

// Read response
let mut buf = vec![0u8; 4096];
let n = tls_client.read(&mut buf).await?;
println!("Response: {}", String::from_utf8_lossy(&buf[..n]));

// Close connection
tls_client.close().await?;
```

### TLS Server

```rust
use starfish_reactor::cooperative_io::tcp_listener::TcpListener;
use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_tls::tls_server::TlsServer;
use rustls::ServerConfig;
use rustls::pki_types::PrivateKeyDer;
use std::sync::Arc;

// Load certificate and private key
let certs = load_certs("server.crt")?;
let key = load_private_key("server.key")?;

// Create server config
let config = ServerConfig::builder()
    .with_no_client_auth()
    .with_single_cert(certs, key)?;

// Accept TCP connection
let mut listener = TcpListener::bind("0.0.0.0:8443").await?;
let tcp_stream = listener.accept().await?;

// Wrap with TLS (handshake happens automatically)
let mut tls_server = TlsServer::new(tcp_stream, config).await?;

// Read request
let mut buf = vec![0u8; 1024];
let n = tls_server.read(&mut buf).await?;

// Send response
tls_server.write_all(b"HTTP/1.1 200 OK\r\n\r\nHello, TLS!").await?;
tls_server.close().await?;
```

### Self-Signed Certificates (Testing)

```rust
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::PrivateKeyDer;
use starfish_tls::accept_any_server_cert_verifier::AcceptAnyServerCertVerifier;

// Generate self-signed certificate
let mut params = CertificateParams::new(vec!["localhost".to_string()])?;
let key_pair = KeyPair::generate()?;
let cert = params.self_signed(&key_pair)?;

// Server config with self-signed cert
let key_der = PrivateKeyDer::from_pem(
    rustls::pki_types::pem::SectionKind::PrivateKey,
    key_pair.serialize_der(),
)?;
let server_config = ServerConfig::builder()
    .with_no_client_auth()
    .with_single_cert(vec![cert.der().to_owned()], key_der)?;

// Client config that accepts any certificate (testing only!)
let mut client_config = ClientConfig::builder()
    .with_root_certificates(RootCertStore::empty())
    .with_no_client_auth();
client_config
    .dangerous()
    .set_certificate_verifier(Arc::new(AcceptAnyServerCertVerifier));
```

## Core Types

### TlsClient<S>

A TLS client wrapper for any async stream.

```rust
pub struct TlsClient<S> {
    connection: ClientConnection,
    stream: S,
}

impl<S: AsyncRead + AsyncWrite> TlsClient<S> {
    /// Create a new TLS client with automatic handshake
    pub async fn new(
        stream: S,
        config: ClientConfig,
        server_name: impl Into<String>,
    ) -> Result<Self, Box<dyn Error>>;
}

// Implements AsyncRead and AsyncWrite
impl<S: AsyncRead + AsyncWrite> AsyncRead for TlsClient<S> { ... }
impl<S: AsyncRead + AsyncWrite> AsyncWrite for TlsClient<S> { ... }
```

### TlsServer<S>

A TLS server wrapper for any async stream.

```rust
pub struct TlsServer<S> {
    connection: ServerConnection,
    stream: S,
}

impl<S: AsyncRead + AsyncWrite> TlsServer<S> {
    /// Create a new TLS server with automatic handshake
    pub async fn new(
        stream: S,
        config: ServerConfig,
    ) -> Result<Self, Box<dyn Error>>;
}

// Implements AsyncRead and AsyncWrite
impl<S: AsyncRead + AsyncWrite> AsyncRead for TlsServer<S> { ... }
impl<S: AsyncRead + AsyncWrite> AsyncWrite for TlsServer<S> { ... }
```

### AcceptAnyServerCertVerifier

A certificate verifier that accepts any server certificate. **For testing only!**

```rust
use starfish_tls::accept_any_server_cert_verifier::AcceptAnyServerCertVerifier;

// WARNING: This disables certificate verification!
// Only use for testing with self-signed certificates.
config.dangerous()
    .set_certificate_verifier(Arc::new(AcceptAnyServerCertVerifier));
```

## AsyncRead/AsyncWrite Traits

Both `TlsClient` and `TlsServer` implement the starfish-reactor async traits:

```rust
// Read with optional timeout
async fn read_with_timeout(
    &mut self,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize>;

// Write with optional timeout
async fn write_with_timeout(
    &mut self,
    buf: &[u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize>;

// Flush with optional timeout
async fn flush_with_timeout(
    &mut self,
    timeout: &Option<IOTimeout>,
) -> io::Result<()>;

// Close the TLS connection (sends close_notify)
async fn close(&mut self) -> io::Result<()>;
```

Extension traits provide convenience methods:
- `read()` - Read without timeout
- `write()` - Write without timeout
- `write_all()` - Write entire buffer
- `flush()` - Flush without timeout

## TLS Handshake

The TLS handshake is performed automatically during `TlsClient::new()` or `TlsServer::new()`. The handshake:

1. Exchanges TLS hello messages
2. Negotiates cipher suite and protocol version
3. Verifies certificates (client verifies server, optionally mutual)
4. Establishes encrypted channel

If the handshake fails, the constructor returns an error.

## Dependencies

```toml
[dependencies]
rustls = "0.23"           # TLS implementation
rustls-pemfile = "2.2"    # PEM file parsing
webpki-roots = "0.26"     # Root CA certificates
rcgen = "0.13"            # Certificate generation (for testing)
starfish-reactor = "0.1"  # Async I/O reactor
```

## Related Crates

| Crate | Description |
|-------|-------------|
| `starfish-reactor` | Async I/O reactor (provides TcpStream, etc.) |
| `starfish-http` | HTTP protocol support |
| `starfish-core` | Core utilities |

## License

See the repository root for license information.
