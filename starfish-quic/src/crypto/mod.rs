//! QUIC packet protection (RFC 9001).
//!
//! Handles derivation of Initial/Handshake/1-RTT keys from rustls,
//! AEAD encryption/decryption of packet payloads, and header protection.

pub mod aead;
pub mod header_protection;
pub mod keys;
