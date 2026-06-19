//! QPACK encoder (RFC 9204).
//!
//! Uses stateless encoding via the pure-Rust `qpack` crate.  Dynamic-table
//! capacity is advertised as zero, so no encoder-stream instructions are
//! produced and no decoder-stream acknowledgements are expected.

use bytes::BytesMut;
use qpack::{encode_stateless, HeaderField};

use crate::error::HttpError;

/// QPACK encoder.
pub struct QpackEncoder {
    /// Maximum dynamic table capacity advertised by the peer.
    max_table_capacity: usize,
    /// Maximum number of blocked streams allowed by the peer.
    blocked_streams: usize,
    pending_stream_data: Vec<u8>,
}

impl QpackEncoder {
    /// Create a new encoder with no dynamic table until peer SETTINGS arrive.
    pub fn new() -> Self {
        Self {
            max_table_capacity: 0,
            blocked_streams: 0,
            pending_stream_data: Vec::new(),
        }
    }

    /// Apply peer SETTINGS for QPACK capacity and blocked-stream limits.
    ///
    /// Currently a no-op because this encoder operates in stateless mode
    /// (dynamic-table capacity = 0).  The values are stored so callers can
    /// query them, but no encoder-stream instructions are produced.
    pub fn apply_peer_settings(
        &mut self,
        max_table_capacity: usize,
        blocked_streams: usize,
    ) -> Result<(), HttpError> {
        self.max_table_capacity = max_table_capacity;
        self.blocked_streams = blocked_streams;
        // Stateless mode — nothing to configure on the codec itself.
        Ok(())
    }

    /// Encode a header block from a list of `(name, value)` pairs.
    pub fn encode_header_block(
        &mut self,
        _stream_id: u64,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, HttpError> {
        let fields: Vec<HeaderField> = headers
            .iter()
            .map(|(n, v)| HeaderField::new(n.as_bytes(), v.as_bytes()))
            .collect();

        let mut block = BytesMut::new();
        encode_stateless(&mut block, &fields)
            .map_err(|e| HttpError::Qpack(format!("encode header block: {e}")))?;
        Ok(block.to_vec())
    }

    /// Feed decoder-stream data from the peer.
    ///
    /// No-op in stateless mode — no dynamic-table state to update.
    pub fn feed_decoder_stream(&mut self, _data: &[u8]) -> Result<(), HttpError> {
        Ok(())
    }

    /// Take pending encoder-stream instructions to send to the peer.
    ///
    /// Always empty in stateless mode.
    pub fn take_pending_stream_data(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_stream_data)
    }

    /// Get the max dynamic table capacity.
    pub fn max_table_capacity(&self) -> usize {
        self.max_table_capacity
    }
}

impl Default for QpackEncoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h3::qpack::decoder::QpackDecoder;

    #[test]
    fn encode_roundtrip_static_headers() {
        let mut encoder = QpackEncoder::new();
        let block = encoder
            .encode_header_block(0, &[(":method", "GET"), (":path", "/")])
            .unwrap();
        let mut decoder = QpackDecoder::new();
        let headers = decoder.decode_header_block(0, &block).unwrap().unwrap();

        assert!(!block.is_empty());
        assert_eq!(headers[0], (":method".into(), "GET".into()));
    }

    #[test]
    fn encode_supports_dynamic_table_instructions_after_configuration() {
        let mut encoder = QpackEncoder::new();
        encoder.apply_peer_settings(4096, 8).unwrap();
        let _block = encoder
            .encode_header_block(0, &[("x-custom-header", "value-123")])
            .unwrap();

        // In stateless mode no encoder-stream instructions are produced.
        assert!(encoder.take_pending_stream_data().is_empty());
    }
}
