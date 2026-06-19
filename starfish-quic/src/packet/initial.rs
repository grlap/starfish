//! Initial packet type (RFC 9000 §17.2.2).
//!
//! Initial packets carry the first CRYPTO frames for the TLS handshake.
//! They are protected with keys derived from the client's first Destination
//! Connection ID.
//!
//! ```text
//! Initial Packet {
//!   Header Form (1) = 1,
//!   Fixed Bit (1) = 1,
//!   Long Packet Type (2) = 0,
//!   Reserved Bits (2),
//!   Packet Number Length (2),
//!   Version (32),
//!   DCID Len (8), DCID (0..160),
//!   SCID Len (8), SCID (0..160),
//!   Token Length (i),
//!   Token (..),
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

/// Parsed Initial packet payload fields (after long header).
#[derive(Debug, Clone, Copy)]
pub struct InitialPacket<'a> {
    pub token: &'a [u8],
    /// Offset in the original buffer where the packet number starts.
    pub pn_offset: usize,
    /// Encoded packet number length (from first byte).
    #[allow(dead_code)]
    pub pn_len: u8,
    /// Total length field value (includes PN + payload + AEAD tag).
    pub length: u64,
}

/// Parse Initial-specific fields from the buffer starting after the long header.
///
/// `header_len` is where the type-specific payload starts (after SCID).
pub fn parse_initial_fields<'a>(
    buf: &'a [u8],
    header_len: usize,
    first_byte: u8,
) -> Result<InitialPacket<'a>, QuicError> {
    let mut offset = header_len;

    // Token Length (varint)
    let (token_len, consumed) = decode_varint(&buf[offset..])?;
    offset += consumed;

    // Token
    let token_len = token_len as usize;
    if buf.len() < offset + token_len {
        return Err(QuicError::InvalidPacket(
            "initial packet truncated at token".into(),
        ));
    }
    let token = &buf[offset..offset + token_len];
    offset += token_len;

    // Length (varint) — covers packet number + payload + AEAD tag
    let (length, consumed) = decode_varint(&buf[offset..])?;
    offset += consumed;

    let pn_len = (first_byte & 0x03) + 1;
    let pn_offset = offset;

    Ok(InitialPacket {
        token,
        pn_offset,
        pn_len,
        length,
    })
}

/// Encode an Initial packet.
///
/// Returns `(packet_bytes, pn_offset, pn_len)`. The caller must AEAD-encrypt
/// the payload (everything after `pn_offset + pn_len`) and then apply header
/// protection.
pub fn encode_initial_packet(
    dcid: &ConnectionId,
    scid: &ConnectionId,
    token: &[u8],
    pn: u64,
    largest_acked: u64,
    payload: &[u8],
    aead_tag_len: usize,
) -> Result<(Vec<u8>, usize, u8), QuicError> {
    let (pn_bytes, pn_len) = encode_packet_number(pn, largest_acked);

    let mut buf = Vec::with_capacity(128 + payload.len());

    // Long header
    encode_long_header(LongPacketType::Initial, dcid, scid, pn_len, &mut buf);

    // Token length + token
    encode_varint(token.len() as u64, &mut buf)?;
    buf.extend_from_slice(token);

    // Length = pn_len + payload + AEAD tag
    let length = pn_len as u64 + payload.len() as u64 + aead_tag_len as u64;
    encode_varint(length, &mut buf)?;

    // Record where PN starts
    let pn_offset = buf.len();

    // Packet number
    buf.extend_from_slice(&pn_bytes);

    // Payload (will be encrypted by caller)
    buf.extend_from_slice(payload);

    Ok((buf, pn_offset, pn_len))
}

/// Minimum size for an Initial packet (RFC 9000 §14.1): 1200 bytes.
/// Pads the packet with PADDING frames (0x00) to meet this requirement.
#[cfg(test)]
pub fn pad_initial_packet(buf: &mut Vec<u8>, min_size: usize) {
    if buf.len() < min_size {
        buf.resize(min_size, 0x00);
    }
}

/// The minimum UDP datagram size containing an Initial packet.
pub const INITIAL_MIN_DATAGRAM_SIZE: usize = 1200;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::header::{parse_header, PacketHeader};

    #[test]
    fn encode_and_parse_initial() {
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let scid = ConnectionId::from_slice(&[10, 20]);

        let payload = vec![0xffu8; 100];
        let (mut pkt, _pn_off, _pn_len) =
            encode_initial_packet(&dcid, &scid, &[], 0, 0, &payload, 16).unwrap();
        pad_initial_packet(&mut pkt, INITIAL_MIN_DATAGRAM_SIZE);

        assert!(pkt.len() >= INITIAL_MIN_DATAGRAM_SIZE);

        // Parse the header
        let header = parse_header(&pkt, 8).unwrap();
        match header {
            PacketHeader::Long(h) => {
                assert_eq!(h.packet_type, LongPacketType::Initial);
                assert_eq!(h.dcid.as_slice(), dcid.as_slice());
                assert_eq!(h.scid.as_slice(), scid.as_slice());

                // Parse Initial-specific fields
                let initial = parse_initial_fields(&pkt, h.header_len, h.first_byte).unwrap();
                assert!(initial.token.is_empty());
                assert!(initial.pn_len >= 1 && initial.pn_len <= 4);
            }
            other => panic!("expected Long header, got {:?}", other),
        }
    }

    #[test]
    fn encode_initial_with_token() {
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let scid = ConnectionId::from_slice(&[5, 6]);
        let token = vec![0xaa, 0xbb, 0xcc, 0xdd];

        let (pkt, _pn_off, _pn_len) =
            encode_initial_packet(&dcid, &scid, &token, 1, 0, &[0u8; 50], 16).unwrap();

        let header = parse_header(&pkt, 4).unwrap();
        match header {
            PacketHeader::Long(h) => {
                let initial = parse_initial_fields(&pkt, h.header_len, h.first_byte).unwrap();
                assert_eq!(initial.token, token);
            }
            other => panic!("expected Long header, got {:?}", other),
        }
    }
}
