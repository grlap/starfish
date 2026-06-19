//! Version Negotiation packet (RFC 9000 §17.2.1).
//!
//! Sent by the server when the client's version is not supported.
//! Version Negotiation packets are not encrypted and have no packet number.

use super::ConnectionId;

/// Encode a Version Negotiation packet.
///
/// ```text
/// Version Negotiation Packet {
///   Header Form (1) = 1,
///   Unused (7),
///   Version (32) = 0,
///   DCID Len (8), DCID (0..160),
///   SCID Len (8), SCID (0..160),
///   Supported Version (32) ...,
/// }
/// ```
pub fn encode_version_negotiation(
    dcid: &ConnectionId,
    scid: &ConnectionId,
    supported_versions: &[u32],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + supported_versions.len() * 4);

    // First byte: form=1, rest unused
    buf.push(0x80);

    // Version = 0 (signals version negotiation)
    buf.extend_from_slice(&[0, 0, 0, 0]);

    // DCID
    buf.push(dcid.len() as u8);
    buf.extend_from_slice(dcid.as_slice());

    // SCID
    buf.push(scid.len() as u8);
    buf.extend_from_slice(scid.as_slice());

    // Supported versions
    for &version in supported_versions {
        buf.extend_from_slice(&version.to_be_bytes());
    }

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::QUIC_VERSION_1;

    #[test]
    fn encode_version_neg() {
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let scid = ConnectionId::from_slice(&[5, 6, 7, 8]);

        let pkt = encode_version_negotiation(&dcid, &scid, &[QUIC_VERSION_1]);

        // Header form bit set
        assert_eq!(pkt[0] & 0x80, 0x80);

        // Version = 0
        assert_eq!(&pkt[1..5], &[0, 0, 0, 0]);

        // DCID
        assert_eq!(pkt[5], 4);
        assert_eq!(&pkt[6..10], &[1, 2, 3, 4]);

        // SCID
        assert_eq!(pkt[10], 4);
        assert_eq!(&pkt[11..15], &[5, 6, 7, 8]);

        // Supported version
        assert_eq!(
            u32::from_be_bytes([pkt[15], pkt[16], pkt[17], pkt[18]]),
            QUIC_VERSION_1
        );
    }
}
