//! CRYPTO frame encoding and decoding (RFC 9000 §19.6).
//!
//! CRYPTO frames carry TLS handshake messages. They use an offset-based
//! delivery model (like STREAM frames) to handle reordering and reassembly.

use crate::error::QuicError;
use crate::packet::{decode_varint, encode_varint};

/// A CRYPTO frame carrying TLS handshake data.
#[derive(Debug, Clone)]
pub struct CryptoFrame {
    /// Byte offset in the crypto stream where this data begins.
    pub offset: u64,
    /// The TLS handshake data.
    pub data: Vec<u8>,
}

impl CryptoFrame {
    /// Encode into a buffer (prepends frame type 0x06).
    pub fn encode(&self, buf: &mut Vec<u8>) -> Result<(), QuicError> {
        encode_varint(0x06, buf)?;
        encode_varint(self.offset, buf)?;
        encode_varint(self.data.len() as u64, buf)?;
        buf.extend_from_slice(&self.data);
        Ok(())
    }
}

/// Decode a CRYPTO frame from the buffer (after the frame type has been consumed).
pub fn decode_crypto_frame(buf: &[u8]) -> Result<(CryptoFrame, usize), QuicError> {
    let mut offset = 0;

    let (crypto_offset, n) = decode_varint(&buf[offset..])?;
    offset += n;

    let (length, n) = decode_varint(&buf[offset..])?;
    offset += n;

    let length = length as usize;
    if buf.len() < offset + length {
        return Err(QuicError::InvalidFrame(
            "CRYPTO frame data truncated".into(),
        ));
    }

    let data = buf[offset..offset + length].to_vec();
    offset += length;

    Ok((
        CryptoFrame {
            offset: crypto_offset,
            data,
        },
        offset,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let frame = CryptoFrame {
            offset: 0,
            data: vec![0x01, 0x00, 0x00, 0xf1], // TLS ClientHello type + length
        };

        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();

        // Skip frame type
        let (ft, type_len) = decode_varint(&buf).unwrap();
        assert_eq!(ft, 0x06);

        let (decoded, consumed) = decode_crypto_frame(&buf[type_len..]).unwrap();
        assert_eq!(decoded.offset, 0);
        assert_eq!(decoded.data, vec![0x01, 0x00, 0x00, 0xf1]);
        assert_eq!(consumed + type_len, buf.len());
    }

    #[test]
    fn nonzero_offset() {
        let frame = CryptoFrame {
            offset: 256,
            data: vec![0xaa; 100],
        };

        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();

        let (_, type_len) = decode_varint(&buf).unwrap();
        let (decoded, _) = decode_crypto_frame(&buf[type_len..]).unwrap();
        assert_eq!(decoded.offset, 256);
        assert_eq!(decoded.data.len(), 100);
    }
}
