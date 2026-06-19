//! QPACK header compression (RFC 9204).
//!
//! QPACK is HTTP/3's header compression format, replacing HPACK from HTTP/2.
//! The wire format still supports the standard static table, optional dynamic
//! table references, and Huffman encoding, but this module currently delegates
//! stateless encoding and decoding to the external `qpack` crate.
//!
//! This implementation currently uses stateless QPACK with Huffman-coded
//! strings and a locally-advertised dynamic-table capacity of zero. That keeps
//! the codec interoperable with peers that honor SETTINGS without yet relying
//! on dynamic-table inserts.
//!
//! Modules:
//! - [`encoder`] — encodes header lists into QPACK wire format
//! - [`decoder`] — decodes QPACK wire format into header lists

pub mod decoder;
pub mod encoder;
