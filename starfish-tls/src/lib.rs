//! Async TLS/SSL support for the Starfish reactor using rustls.
//!
//! Provides `TlsClient` and `TlsServer` wrappers that add TLS encryption
//! to any async stream implementing `AsyncRead + AsyncWrite`. Uses rustls
//! for pure-Rust cryptographic operations without OpenSSL dependency.

pub mod accept_any_server_cert_verifier;
pub mod error;
pub mod tls_client;
pub mod tls_server;

/// Minimum size for TLS read buffers. Chosen to comfortably hold
/// a full TLS record (max 16 KB) plus overhead.
const MIN_READ_BUF_SIZE: usize = 16384;
