//! QUIC transport protocol implementation (RFC 9000).
//!
//! Provides a from-scratch QUIC implementation built on `starfish-reactor`'s
//! `UdpSocket` and `rustls` for TLS 1.3 integration (RFC 9001). Includes
//! RFC 9002-compliant loss detection and congestion control (NewReno + Cubic).
//!
//! # Key Types
//!
//! - [`QuicConnection`] — a QUIC connection (client or server side)
//! - [`QuicStream`] — a single QUIC stream implementing `AsyncRead + AsyncWrite`
//! - [`QuicClient`] — initiates QUIC connections
//! - [`QuicListener`] — accepts incoming QUIC connections (single-connection)
//! - [`QuicEndpoint`] — multi-connection server with DCID routing
//!
//! # Modules
//!
//! | Module | Description |
//! |--------|-------------|
//! | `packet` | Wire-format packet parsing and serialization (RFC 9000 §17) |
//! | `frame` | QUIC frame encode/decode (RFC 9000 §19) |
//! | `crypto` | Packet protection, AEAD, header protection (RFC 9001) |
//! | `transport` | Connection IDs, flow control, stream state machines |
//! | `recovery` | Loss detection and congestion control (RFC 9002) |

pub(crate) mod client;
pub(crate) mod connection;
pub(crate) mod crypto;
pub(crate) mod error;
pub(crate) mod frame;
pub(crate) mod packet;
pub(crate) mod recovery;
pub(crate) mod server;
pub(crate) mod stream;
pub(crate) mod transport;

// Re-export key types at crate root for convenience.
pub use client::{QuicClient, QuicClientConfig};
pub use connection::{ConnectionState, QuicConnection};
pub use error::{QuicError, TransportErrorCode};
pub use packet::{decode_varint, encode_varint};
pub use server::{ConnectionHandle, QuicEndpoint, QuicListener, QuicServerConfig};
pub use stream::QuicStream;
pub use transport::stream_manager::StreamId;
