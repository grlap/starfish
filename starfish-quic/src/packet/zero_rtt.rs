//! 0-RTT packet type (RFC 9000 §17.2.3).
//!
//! 0-RTT packets carry early application data from the client on a resumed
//! connection. They share the long-header layout used by Handshake packets.

use super::{
    decode_varint, encode_packet_number, encode_varint,
    header::{encode_long_header, LongPacketType},
    ConnectionId,
};
use crate::error::QuicError;

/// Parsed 0-RTT packet fields (after long header).
#[derive(Debug, Clone)]
pub struct ZeroRttPacket {
    /// Offset in the original buffer where the packet number starts.
    pub pn_offset: usize,
    /// Encoded packet number length (from first byte).
    #[allow(dead_code)]
    pub pn_len: u8,
    /// Total length field value (includes PN + payload + AEAD tag).
    pub length: u64,
}

/// Parse 0-RTT-specific fields from the buffer starting after the long header.
pub fn parse_zero_rtt_fields(
    buf: &[u8],
    header_len: usize,
    first_byte: u8,
) -> Result<ZeroRttPacket, QuicError> {
    let mut offset = header_len;

    let (length, consumed) = decode_varint(&buf[offset..])?;
    offset += consumed;

    let pn_len = (first_byte & 0x03) + 1;
    let pn_offset = offset;

    Ok(ZeroRttPacket {
        pn_offset,
        pn_len,
        length,
    })
}

/// Encode a 0-RTT packet.
///
/// Returns `(packet_bytes, pn_offset, pn_len)`.
pub fn encode_zero_rtt_packet(
    dcid: &ConnectionId,
    scid: &ConnectionId,
    pn: u64,
    largest_acked: u64,
    payload: &[u8],
    aead_tag_len: usize,
) -> Result<(Vec<u8>, usize, u8), QuicError> {
    let (pn_bytes, pn_len) = encode_packet_number(pn, largest_acked);

    let mut buf = Vec::with_capacity(64 + payload.len());

    encode_long_header(LongPacketType::ZeroRtt, dcid, scid, pn_len, &mut buf);

    let length = pn_len as u64 + payload.len() as u64 + aead_tag_len as u64;
    encode_varint(length, &mut buf)?;

    let pn_offset = buf.len();
    buf.extend_from_slice(&pn_bytes);
    buf.extend_from_slice(payload);

    Ok((buf, pn_offset, pn_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::header::{parse_header, PacketHeader};

    #[test]
    fn encode_and_parse_zero_rtt() {
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let scid = ConnectionId::from_slice(&[5, 6, 7, 8]);

        let payload = vec![0xab; 32];
        let (pkt, _pn_off, _pn_len) =
            encode_zero_rtt_packet(&dcid, &scid, 7, 0, &payload, 16).unwrap();

        match parse_header(&pkt, 4).unwrap() {
            PacketHeader::Long(h) => {
                assert_eq!(h.packet_type, LongPacketType::ZeroRtt);
                let zero_rtt = parse_zero_rtt_fields(&pkt, h.header_len, h.first_byte).unwrap();
                assert!(zero_rtt.pn_len >= 1 && zero_rtt.pn_len <= 4);
                assert_eq!(zero_rtt.length, zero_rtt.pn_len as u64 + 32 + 16);
            }
            other => panic!("expected Long header, got {:?}", other),
        }
    }
}
