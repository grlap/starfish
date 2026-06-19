//! STREAM frame encoding and decoding (RFC 9000 §19.8).
//!
//! STREAM frames carry application data. The frame type byte encodes
//! three flags: OFF (offset present), LEN (length present), FIN (final data).
//!
//! ```text
//! STREAM Frame {
//!   Type (i) = 0x08..0x0f,
//!   Stream ID (i),
//!   [Offset (i)],
//!   [Length (i)],
//!   Stream Data (..),
//! }
//! ```
//!
//! Type bits: 0x08 | OFF(0x04) | LEN(0x02) | FIN(0x01)

use crate::error::QuicError;
use crate::packet::{decode_varint, encode_varint};

/// A STREAM frame carrying application data.
#[derive(Debug, Clone)]
pub struct StreamFrame {
    pub stream_id: u64,
    /// Byte offset in the stream. 0 if the OFF bit is not set.
    pub offset: u64,
    /// true if this is the final data on the stream (FIN bit).
    pub fin: bool,
    /// The stream data payload.
    pub data: Vec<u8>,
}

impl StreamFrame {
    /// Encode into a buffer.
    ///
    /// Always sets the OFF and LEN bits for unambiguous framing.
    pub fn encode(&self, buf: &mut Vec<u8>) -> Result<(), QuicError> {
        let mut type_byte = 0x08u8;
        // Always include offset (OFF bit)
        type_byte |= 0x04;
        // Always include length (LEN bit)
        type_byte |= 0x02;
        if self.fin {
            type_byte |= 0x01;
        }

        encode_varint(type_byte as u64, buf)?;
        encode_varint(self.stream_id, buf)?;
        encode_varint(self.offset, buf)?;
        encode_varint(self.data.len() as u64, buf)?;
        buf.extend_from_slice(&self.data);
        Ok(())
    }
}

/// Decode a STREAM frame. `type_byte` is the already-parsed frame type (0x08..0x0f).
pub fn decode_stream_frame(buf: &[u8], type_byte: u8) -> Result<(StreamFrame, usize), QuicError> {
    let has_offset = type_byte & 0x04 != 0;
    let has_length = type_byte & 0x02 != 0;
    let fin = type_byte & 0x01 != 0;

    let mut pos = 0;

    let (stream_id, n) = decode_varint(&buf[pos..])?;
    pos += n;

    let offset = if has_offset {
        let (off, n) = decode_varint(&buf[pos..])?;
        pos += n;
        off
    } else {
        0
    };

    let data = if has_length {
        let (length, n) = decode_varint(&buf[pos..])?;
        pos += n;
        let length = length as usize;
        if buf.len() < pos + length {
            return Err(QuicError::InvalidFrame(
                "STREAM frame data truncated".into(),
            ));
        }
        let d = buf[pos..pos + length].to_vec();
        pos += length;
        d
    } else {
        // No length field — data extends to end of packet
        let d = buf[pos..].to_vec();
        pos = buf.len();
        d
    };

    Ok((
        StreamFrame {
            stream_id,
            offset,
            fin,
            data,
        },
        pos,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_all_bits() {
        let frame = StreamFrame {
            stream_id: 4,
            offset: 100,
            fin: true,
            data: vec![0x48, 0x65, 0x6c, 0x6c, 0x6f], // "Hello"
        };

        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();

        // Parse type byte
        let (type_val, type_len) = decode_varint(&buf).unwrap();
        let type_byte = type_val as u8;
        assert_eq!(type_byte & 0x04, 0x04); // OFF
        assert_eq!(type_byte & 0x02, 0x02); // LEN
        assert_eq!(type_byte & 0x01, 0x01); // FIN

        let (decoded, _) = decode_stream_frame(&buf[type_len..], type_byte).unwrap();
        assert_eq!(decoded.stream_id, 4);
        assert_eq!(decoded.offset, 100);
        assert!(decoded.fin);
        assert_eq!(decoded.data, b"Hello");
    }

    #[test]
    fn decode_no_offset_no_length() {
        // Construct a STREAM frame with no OFF, no LEN, no FIN: type = 0x08
        let type_byte = 0x08u8;
        let mut buf = Vec::new();
        encode_varint(0, &mut buf).unwrap(); // stream_id = 0
        buf.extend_from_slice(b"data"); // rest is data

        let (decoded, consumed) = decode_stream_frame(&buf, type_byte).unwrap();
        assert_eq!(decoded.stream_id, 0);
        assert_eq!(decoded.offset, 0);
        assert!(!decoded.fin);
        assert_eq!(decoded.data, b"data");
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn zero_length_fin() {
        let frame = StreamFrame {
            stream_id: 8,
            offset: 500,
            fin: true,
            data: vec![],
        };

        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();

        let (type_val, type_len) = decode_varint(&buf).unwrap();
        let (decoded, _) = decode_stream_frame(&buf[type_len..], type_val as u8).unwrap();
        assert_eq!(decoded.stream_id, 8);
        assert_eq!(decoded.offset, 500);
        assert!(decoded.fin);
        assert!(decoded.data.is_empty());
    }
}
