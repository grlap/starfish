//! HTTP/3 implementation (RFC 9114).
//!
//! Modules:
//! - [`frame`] — HTTP/3 frame encoding and decoding
//! - [`qpack`] — QPACK header compression (RFC 9204)
//! - [`connection`] — HTTP/3 connection management
//! - [`client`] — HTTP/3 client
//! - [`server`] — HTTP/3 server

pub(crate) const H3_ALPN: &[u8] = b"h3";

pub(crate) fn force_h3_alpn(protocols: &mut Vec<Vec<u8>>) {
    protocols.clear();
    protocols.push(H3_ALPN.to_vec());
}

pub mod client;
pub mod connection;
pub mod error;
pub mod frame;
pub mod qpack;
pub mod server;
