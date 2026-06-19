//! QUIC packet header parsing and serialization (RFC 9000 §17).
//!
//! Long headers (§17.2) are used during connection establishment (Initial,
//! Handshake, 0-RTT, Retry). Short headers (§17.3) are used for 1-RTT packets
//! after the handshake completes.

use super::{ConnectionId, QUIC_VERSION_1};
use crate::error::QuicError;

/// Packet types for long-header packets (RFC 9000 §17.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongPacketType {
    Initial = 0x00,
    ZeroRtt = 0x01,
    Handshake = 0x02,
    Retry = 0x03,
}

impl LongPacketType {
    pub fn from_bits(bits: u8) -> Result<Self, QuicError> {
        match bits {
            0x00 => Ok(Self::Initial),
            0x01 => Ok(Self::ZeroRtt),
            0x02 => Ok(Self::Handshake),
            0x03 => Ok(Self::Retry),
            _ => Err(QuicError::InvalidPacket(format!(
                "unknown long packet type: {bits:#04x}"
            ))),
        }
    }
}

/// Packet number space — determines which crypto keys protect the packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketNumberSpace {
    Initial = 0,
    Handshake = 1,
    ApplicationData = 2,
}

/// A parsed long header (RFC 9000 §17.2).
///
/// ```text
/// Long Header Packet {
///   Header Form (1) = 1,
///   Fixed Bit (1) = 1,
///   Long Packet Type (2),
///   Type-Specific Bits (4),
///   Version (32),
///   Destination Connection ID Length (8),
///   Destination Connection ID (0..160),
///   Source Connection ID Length (8),
///   Source Connection ID (0..160),
///   Type-Specific Payload (..),
/// }
/// ```
#[derive(Debug, Clone)]
pub struct LongHeader {
    pub packet_type: LongPacketType,
    /// The full first byte (needed for AEAD AAD and header protection).
    pub first_byte: u8,
    pub version: u32,
    pub dcid: ConnectionId,
    pub scid: ConnectionId,
    /// Byte offset where the type-specific payload begins.
    pub header_len: usize,
}

/// A parsed short header / 1-RTT packet header (RFC 9000 §17.3).
///
/// ```text
/// 1-RTT Packet {
///   Header Form (1) = 0,
///   Fixed Bit (1) = 1,
///   Spin Bit (1),
///   Reserved Bits (2),
///   Key Phase (1),
///   Packet Number Length (2),
///   Destination Connection ID (0..160),
///   Packet Number (8..32),
///   Packet Payload (..),
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ShortHeader {
    #[allow(dead_code)]
    pub first_byte: u8,
    #[allow(dead_code)]
    pub spin_bit: bool,
    #[allow(dead_code)]
    pub key_phase: bool,
    pub dcid: ConnectionId,
    /// Byte offset where the packet number begins.
    #[allow(dead_code)]
    pub header_len: usize,
}

/// Result of parsing a packet's header.
#[derive(Debug)]
pub enum PacketHeader {
    Long(LongHeader),
    Short(ShortHeader),
    VersionNegotiation {
        #[allow(dead_code)]
        dcid: ConnectionId,
        #[allow(dead_code)]
        scid: ConnectionId,
        supported_versions: Vec<u32>,
    },
}

/// Determine whether a packet uses the long header form.
pub fn is_long_header(first_byte: u8) -> bool {
    first_byte & 0x80 != 0
}

/// Parse a packet header from a UDP datagram.
///
/// Does NOT decrypt the packet or remove header protection — the caller
/// must do that using the appropriate crypto keys.
pub fn parse_header(buf: &[u8], local_cid_len: usize) -> Result<PacketHeader, QuicError> {
    if buf.is_empty() {
        return Err(QuicError::InvalidPacket("empty packet".into()));
    }

    let first_byte = buf[0];

    if is_long_header(first_byte) {
        parse_long_header(buf)
    } else {
        parse_short_header(buf, local_cid_len)
    }
}

fn parse_long_header(buf: &[u8]) -> Result<PacketHeader, QuicError> {
    // Minimum long header: 1 (first) + 4 (version) + 1 (dcid_len) + 1 (scid_len) = 7
    if buf.len() < 7 {
        return Err(QuicError::InvalidPacket("long header too short".into()));
    }

    let first_byte = buf[0];
    let version = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);

    let dcid_len = buf[5] as usize;
    if dcid_len > 20 || buf.len() < 6 + dcid_len + 1 {
        return Err(QuicError::InvalidPacket("invalid DCID length".into()));
    }
    let dcid = ConnectionId::from_slice(&buf[6..6 + dcid_len]);

    let scid_len = buf[6 + dcid_len] as usize;
    let scid_start = 7 + dcid_len;
    if scid_len > 20 || buf.len() < scid_start + scid_len {
        return Err(QuicError::InvalidPacket("invalid SCID length".into()));
    }
    let scid = ConnectionId::from_slice(&buf[scid_start..scid_start + scid_len]);

    let header_len = scid_start + scid_len;

    // Version 0 = version negotiation
    if version == 0 {
        let mut offset = header_len;
        let mut supported_versions = Vec::new();
        while offset + 4 <= buf.len() {
            let v = u32::from_be_bytes([
                buf[offset],
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
            ]);
            supported_versions.push(v);
            offset += 4;
        }
        return Ok(PacketHeader::VersionNegotiation {
            dcid,
            scid,
            supported_versions,
        });
    }

    let packet_type = LongPacketType::from_bits((first_byte & 0x30) >> 4)?;

    Ok(PacketHeader::Long(LongHeader {
        packet_type,
        first_byte,
        version,
        dcid,
        scid,
        header_len,
    }))
}

fn parse_short_header(buf: &[u8], local_cid_len: usize) -> Result<PacketHeader, QuicError> {
    // Minimum: 1 (first) + dcid_len + 1 (pn, at least 1 byte)
    if buf.len() < 1 + local_cid_len + 1 {
        return Err(QuicError::InvalidPacket("short header too short".into()));
    }

    let first_byte = buf[0];
    let spin_bit = first_byte & 0x20 != 0;
    let key_phase = first_byte & 0x04 != 0;
    let dcid = ConnectionId::from_slice(&buf[1..1 + local_cid_len]);
    let header_len = 1 + local_cid_len;

    Ok(PacketHeader::Short(ShortHeader {
        first_byte,
        spin_bit,
        key_phase,
        dcid,
        header_len,
    }))
}

/// Encode a long header into a buffer. Returns the header length.
///
/// Does NOT include the packet number or payload — the caller appends those.
pub fn encode_long_header(
    packet_type: LongPacketType,
    dcid: &ConnectionId,
    scid: &ConnectionId,
    pn_len: u8,
    buf: &mut Vec<u8>,
) -> usize {
    let start = buf.len();

    // First byte: form=1, fixed=1, type (2 bits), reserved+pn_len (4 bits)
    let first_byte = 0xc0 | ((packet_type as u8) << 4) | (pn_len - 1);
    buf.push(first_byte);

    // Version
    buf.extend_from_slice(&QUIC_VERSION_1.to_be_bytes());

    // DCID
    buf.push(dcid.len() as u8);
    buf.extend_from_slice(dcid.as_slice());

    // SCID
    buf.push(scid.len() as u8);
    buf.extend_from_slice(scid.as_slice());

    buf.len() - start
}

/// Encode a short (1-RTT) header. Returns the header length.
pub fn encode_short_header(
    dcid: &ConnectionId,
    spin_bit: bool,
    key_phase: bool,
    pn_len: u8,
    buf: &mut Vec<u8>,
) -> usize {
    let start = buf.len();

    let mut first_byte = 0x40; // form=0, fixed=1
    if spin_bit {
        first_byte |= 0x20;
    }
    if key_phase {
        first_byte |= 0x04;
    }
    first_byte |= (pn_len - 1) & 0x03;
    buf.push(first_byte);

    buf.extend_from_slice(dcid.as_slice());

    buf.len() - start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_initial_header() {
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let scid = ConnectionId::from_slice(&[10, 20, 30, 40]);

        let mut buf = Vec::new();
        encode_long_header(LongPacketType::Initial, &dcid, &scid, 1, &mut buf);
        // Add token length (varint 0) + length (varint) + PN + payload for a valid Initial
        buf.push(0x00); // token length = 0
        buf.extend_from_slice(&[0x41, 0x00]); // length = 256 (varint)
        buf.push(0x00); // packet number
        buf.extend(vec![0u8; 255]); // payload

        match parse_header(&buf, 8).unwrap() {
            PacketHeader::Long(h) => {
                assert_eq!(h.packet_type, LongPacketType::Initial);
                assert_eq!(h.version, QUIC_VERSION_1);
                assert_eq!(h.dcid.as_slice(), &[1, 2, 3, 4, 5, 6, 7, 8]);
                assert_eq!(h.scid.as_slice(), &[10, 20, 30, 40]);
            }
            other => panic!("expected Long header, got {:?}", other),
        }
    }

    #[test]
    fn parse_short_header() {
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let mut buf = Vec::new();
        encode_short_header(&dcid, false, false, 1, &mut buf);
        buf.push(0x42); // packet number
        buf.extend(vec![0u8; 16]); // payload

        match super::parse_header(&buf, 4).unwrap() {
            PacketHeader::Short(h) => {
                assert_eq!(h.dcid.as_slice(), &[1, 2, 3, 4]);
                assert!(!h.spin_bit);
                assert!(!h.key_phase);
            }
            other => panic!("expected Short header, got {:?}", other),
        }
    }

    #[test]
    fn version_negotiation() {
        let mut buf = Vec::new();
        buf.push(0x80); // long header form
        buf.extend_from_slice(&[0, 0, 0, 0]); // version = 0
        buf.push(4); // dcid len
        buf.extend_from_slice(&[1, 2, 3, 4]); // dcid
        buf.push(4); // scid len
        buf.extend_from_slice(&[5, 6, 7, 8]); // scid
        buf.extend_from_slice(&QUIC_VERSION_1.to_be_bytes()); // supported version

        match parse_header(&buf, 4).unwrap() {
            PacketHeader::VersionNegotiation {
                dcid,
                scid,
                supported_versions,
            } => {
                assert_eq!(dcid.as_slice(), &[1, 2, 3, 4]);
                assert_eq!(scid.as_slice(), &[5, 6, 7, 8]);
                assert_eq!(supported_versions, vec![QUIC_VERSION_1]);
            }
            other => panic!("expected VersionNegotiation, got {:?}", other),
        }
    }

    #[test]
    fn short_header_spin_and_key_phase() {
        let dcid = ConnectionId::from_slice(&[0xaa]);
        let mut buf = Vec::new();
        encode_short_header(&dcid, true, true, 2, &mut buf);
        buf.extend_from_slice(&[0x00, 0x01]); // packet number (2 bytes)
        buf.extend(vec![0u8; 16]); // payload

        match super::parse_header(&buf, 1).unwrap() {
            PacketHeader::Short(h) => {
                assert!(h.spin_bit);
                assert!(h.key_phase);
            }
            other => panic!("expected Short header, got {:?}", other),
        }
    }
}
