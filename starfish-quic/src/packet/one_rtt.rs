//! 1-RTT (short header) packets (RFC 9000 §17.3).
//!
//! Used for application data after the handshake completes. Short headers
//! contain only the DCID (no SCID, no version) for minimal overhead.

use super::{encode_packet_number, header::encode_short_header, ConnectionId};

/// Encode a 1-RTT packet with a short header.
///
/// Returns `(packet_bytes, pn_offset, pn_len)`. The caller must AEAD-encrypt
/// the payload and apply header protection.
pub fn encode_one_rtt_packet(
    dcid: &ConnectionId,
    spin_bit: bool,
    key_phase: bool,
    pn: u64,
    largest_acked: u64,
    payload: &[u8],
) -> (Vec<u8>, usize, u8) {
    let (pn_bytes, pn_len) = encode_packet_number(pn, largest_acked);

    let mut buf = Vec::with_capacity(1 + dcid.len() + pn_len as usize + payload.len());

    encode_short_header(dcid, spin_bit, key_phase, pn_len, &mut buf);

    let pn_offset = buf.len();

    // Packet number
    buf.extend_from_slice(&pn_bytes);

    // Payload (will be encrypted by caller)
    buf.extend_from_slice(payload);

    (buf, pn_offset, pn_len)
}

/// Returns the offset where the packet number begins in a 1-RTT packet.
pub fn one_rtt_pn_offset(dcid_len: usize) -> usize {
    1 + dcid_len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::header::{parse_header, PacketHeader};

    #[test]
    fn encode_and_parse_one_rtt() {
        let dcid = ConnectionId::from_slice(&[0x01, 0x02, 0x03, 0x04]);
        let payload = vec![0xffu8; 32];

        let (pkt, _pn_off, _pn_len) = encode_one_rtt_packet(&dcid, false, false, 42, 40, &payload);

        match parse_header(&pkt, 4).unwrap() {
            PacketHeader::Short(h) => {
                assert_eq!(h.dcid.as_slice(), dcid.as_slice());
                assert!(!h.spin_bit);
                assert!(!h.key_phase);
                assert_eq!(h.header_len, 5); // 1 byte + 4 CID bytes
            }
            other => panic!("expected Short header, got {:?}", other),
        }
    }

    #[test]
    fn one_rtt_with_spin_and_key_phase() {
        let dcid = ConnectionId::from_slice(&[0xaa]);
        let (pkt, _pn_off, _pn_len) = encode_one_rtt_packet(&dcid, true, true, 100, 99, &[0u8; 16]);

        match parse_header(&pkt, 1).unwrap() {
            PacketHeader::Short(h) => {
                assert!(h.spin_bit);
                assert!(h.key_phase);
            }
            other => panic!("expected Short header, got {:?}", other),
        }
    }
}
