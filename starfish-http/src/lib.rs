//! HTTP/1.1 and HTTP/3 for the Starfish async runtime.
//!
//! This crate provides:
//! - **HTTP/1.1** (RFC 9112): incremental parser, encoder, chunked transfer,
//!   keep-alive connection management, client and server.
//! - **HTTP/3** (RFC 9114): frame codec, QPACK header compression (RFC 9204),
//!   client and server over QUIC (`starfish-quic`).
//! - **Unified layer**: `HttpClient` with auto protocol selection and
//!   connection pooling; `HttpServer` for HTTP/1.1 over TCP.
//!
//! All I/O is built on `starfish-reactor`'s `AsyncRead` / `AsyncWrite` traits.

pub mod error;
pub mod h1;
pub mod h3;

pub mod client;
mod content_coding;
pub mod pool;
pub mod server;

pub use client::{HttpClient, ProxyConfig, ProxyKind};
pub use error::HttpError;
pub use h1::client::H1Client;
pub use h1::server::H1Server;
pub use h3::client::{H3Client, H3Priority};
pub use h3::connection::H3Push;
pub use h3::server::{H3Server, H3ServerConnection};
pub use pool::ConnectionPool;
pub use server::HttpServer;
