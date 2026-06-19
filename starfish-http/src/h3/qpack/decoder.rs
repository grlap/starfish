//! QPACK decoder (RFC 9204).
//!
//! Uses stateless decoding via the pure-Rust `qpack` crate.  Dynamic-table
//! capacity is advertised as zero, so no decoder-stream instructions are
//! produced and no encoder-stream data is expected.

use bytes::Bytes;
use qpack::decode_stateless;

use crate::error::HttpError;

/// Maximum decoded header-list size passed to `decode_stateless`.
const MAX_HEADER_LIST_SIZE: u64 = 256 * 1024;

/// QPACK decoder.
pub struct QpackDecoder {
    /// Maximum dynamic table capacity.
    max_table_capacity: usize,
    _blocked_streams: usize,
    pending_stream_data: Vec<u8>,
}

impl QpackDecoder {
    /// Create a new decoder with no dynamic table.
    pub fn new() -> Self {
        Self::with_capacity(0, 0)
    }

    /// Create a decoder with the given dynamic-table capacity and blocked-stream limit.
    ///
    /// In stateless mode the capacity and blocked-stream values are stored but
    /// do not affect decoding — only the static table is used.
    pub fn with_capacity(capacity: usize, blocked_streams: usize) -> Self {
        Self {
            max_table_capacity: capacity,
            _blocked_streams: blocked_streams,
            pending_stream_data: Vec::new(),
        }
    }

    /// Get the max dynamic table capacity.
    pub fn max_table_capacity(&self) -> usize {
        self.max_table_capacity
    }

    /// Feed encoder-stream data from the peer.
    ///
    /// No-op in stateless mode — no dynamic-table state to update.
    pub fn feed_encoder_stream(&mut self, _data: &[u8]) -> Result<(), HttpError> {
        Ok(())
    }

    /// Decode a QPACK header block into a list of `(name, value)` pairs.
    ///
    /// Returns `Ok(None)` when the block is temporarily blocked on dynamic-table
    /// state and must be retried after more encoder-stream data arrives.
    /// In stateless mode this never happens — blocks always decode immediately.
    pub fn decode_header_block(
        &mut self,
        _stream_id: u64,
        data: &[u8],
    ) -> Result<Option<Vec<(String, String)>>, HttpError> {
        let mut buf = Bytes::copy_from_slice(data);
        let decoded = decode_stateless(&mut buf, MAX_HEADER_LIST_SIZE)
            .map_err(|e| HttpError::Qpack(format!("decode header block: {e}")))?;

        let headers = decoded
            .fields
            .iter()
            .map(|f| {
                let name = String::from_utf8_lossy(f.name.as_ref()).into_owned();
                let value = String::from_utf8_lossy(f.value.as_ref()).into_owned();
                (name, value)
            })
            .collect();

        Ok(Some(headers))
    }

    /// Retry a previously-blocked header block after more encoder-stream data arrived.
    ///
    /// In stateless mode there is no blocking, so this always returns `Ok(None)`.
    pub fn try_unblocked(
        &mut self,
        _stream_id: u64,
    ) -> Result<Option<Vec<(String, String)>>, HttpError> {
        Ok(None)
    }

    /// Take pending decoder-stream instructions to send to the peer.
    pub fn take_pending_stream_data(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_stream_data)
    }
}

impl Default for QpackDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h3::qpack::encoder::QpackEncoder;

    #[test]
    fn decode_static_headers_roundtrip() {
        let mut encoder = QpackEncoder::new();
        let block = encoder
            .encode_header_block(0, &[(":method", "GET"), (":path", "/")])
            .unwrap();

        let mut decoder = QpackDecoder::new();
        let headers = decoder.decode_header_block(0, &block).unwrap().unwrap();
        assert_eq!(headers[0], (":method".into(), "GET".into()));
        assert_eq!(headers[1], (":path".into(), "/".into()));
    }

    #[test]
    fn decode_huffman_literal_headers() {
        let mut encoder = QpackEncoder::new();
        let block = encoder
            .encode_header_block(
                0,
                &[(":authority", "example.com"), ("user-agent", "starfish")],
            )
            .unwrap();

        let mut decoder = QpackDecoder::new();
        let headers = decoder.decode_header_block(0, &block).unwrap().unwrap();

        assert!(headers.contains(&(":authority".into(), "example.com".into())));
        assert!(headers.contains(&("user-agent".into(), "starfish".into())));
    }

    #[test]
    fn decode_dynamic_headers_stateless_fallback() {
        // In stateless mode, apply_peer_settings is a no-op and encoding
        // still works (just without dynamic-table compression).
        let mut encoder = QpackEncoder::new();
        encoder.apply_peer_settings(4096, 8).unwrap();
        let block = encoder
            .encode_header_block(0, &[("x-dynamic", "value")])
            .unwrap();

        let mut decoder = QpackDecoder::with_capacity(4096, 8);
        let headers = decoder.decode_header_block(0, &block).unwrap().unwrap();
        assert!(headers.contains(&("x-dynamic".into(), "value".into())));
    }
}
