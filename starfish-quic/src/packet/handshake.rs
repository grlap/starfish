//! Handshake packet type (RFC 9000 §17.2.4).
//!
//! Handshake packets carry TLS handshake messages after the Initial exchange.
//! Protected with Handshake-level keys derived from the TLS handshake.
//!
//! ```text
//! Handshake Packet {
//!   Header Form (1) = 1,
//!   Fixed Bit (1) = 1,
//!   Long Packet Type (2) = 2,
//!   Reserved Bits (2),
//!   Packet Number Length (2),
//!   Version (32),
//!   DCID Len (8), DCID (0..160),
//!   SCID Len (8), SCID (0..160),
//!   Length (i),
//!   Packet Number (8..32),
//!   Packet Payload (..),
//! }
//! ```

use super::{
    decode_varint, encode_packet_number, encode_varint,
    header::{encode_long_header, LongPacketType},
    ConnectionId,
};
use crate::error::QuicError;

/// Parsed Handshake packet fields (after long header).
#[derive(Debug, Clone)]
pub struct HandshakePacket {
    /// Offset in the original buffer where the packet number starts.
    pub pn_offset: usize,
    /// Encoded packet number length (from first byte).
    #[allow(dead_code)]
    pub pn_len: u8,
    /// Total length field value (includes PN + payload + AEAD tag).
    pub length: u64,
}

/// Parse Handshake-specific fields from the buffer starting after the long header.
pub fn parse_handshake_fields(
    buf: &[u8],
    header_len: usize,
    first_byte: u8,
) -> Result<HandshakePacket, QuicError> {
    let mut offset = header_len;

    // Length (varint) — covers packet number + payload + AEAD tag
    let (length, consumed) = decode_varint(&buf[offset..])?;
    offset += consumed;

    let pn_len = (first_byte & 0x03) + 1;
    let pn_offset = offset;

    Ok(HandshakePacket {
        pn_offset,
        pn_len,
        length,
    })
}

/// Encode a Handshake packet.
///
/// Returns `(packet_bytes, pn_offset, pn_len)`.
pub fn encode_handshake_packet(
    dcid: &ConnectionId,
    scid: &ConnectionId,
    pn: u64,
    largest_acked: u64,
    payload: &[u8],
    aead_tag_len: usize,
) -> Result<(Vec<u8>, usize, u8), QuicError> {
    let (pn_bytes, pn_len) = encode_packet_number(pn, largest_acked);

    let mut buf = Vec::with_capacity(64 + payload.len());

    // Long header
    encode_long_header(LongPacketType::Handshake, dcid, scid, pn_len, &mut buf);

    // Length = pn_len + payload + AEAD tag
    let length = pn_len as u64 + payload.len() as u64 + aead_tag_len as u64;
    encode_varint(length, &mut buf)?;

    let pn_offset = buf.len();

    // Packet number
    buf.extend_from_slice(&pn_bytes);

    // Payload (will be encrypted by caller)
    buf.extend_from_slice(payload);

    Ok((buf, pn_offset, pn_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::header::{parse_header, PacketHeader};

    #[test]
    fn encode_and_parse_handshake() {
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let scid = ConnectionId::from_slice(&[5, 6, 7, 8]);

        let payload = vec![0xab; 64];
        let (pkt, _pn_off, _pn_len) =
            encode_handshake_packet(&dcid, &scid, 0, 0, &payload, 16).unwrap();

        match parse_header(&pkt, 4).unwrap() {
            PacketHeader::Long(h) => {
                assert_eq!(h.packet_type, LongPacketType::Handshake);
                let hs = parse_handshake_fields(&pkt, h.header_len, h.first_byte).unwrap();
                assert!(hs.pn_len >= 1 && hs.pn_len <= 4);
                assert_eq!(hs.length, hs.pn_len as u64 + 64 + 16);
            }
            other => panic!("expected Long header, got {:?}", other),
        }
    }
}
