//! QUIC packet parsing and serialization (RFC 9000 §17).
//!
//! QUIC packets use either a long header (Initial, Handshake, 0-RTT, Retry)
//! or a short header (1-RTT) format. This module provides types for parsing
//! incoming packets and serializing outgoing packets.
//!
//! # Packet Number Encoding
//!
//! Packet numbers use variable-length encoding (1–4 bytes). The sender uses
//! the minimum length that allows the receiver to reconstruct the full number
//! given the largest acknowledged packet number.

pub mod handshake;
pub mod header;
pub mod initial;
pub mod one_rtt;
pub mod retry;
pub mod version_negotiation;
pub mod zero_rtt;

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::atomic::{AtomicU64, Ordering};

/// QUIC version 1 (RFC 9000).
pub const QUIC_VERSION_1: u32 = 0x00000001;

/// Maximum connection ID length (RFC 9000 §17.2).
pub const MAX_CID_LEN: usize = 20;
/// Maximum QUIC variable-length integer value (RFC 9000 §16).
pub const MAX_VARINT: u64 = (1u64 << 62) - 1;
/// Maximum ACK delay exponent accepted from peer transport parameters.
pub const MAX_ACK_DELAY_EXPONENT: u32 = 20;

/// A QUIC connection ID.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ConnectionId {
    len: u8,
    bytes: [u8; MAX_CID_LEN],
}

impl ConnectionId {
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            len: 0,
            bytes: [0; MAX_CID_LEN],
        }
    }

    pub fn from_slice(data: &[u8]) -> Self {
        debug_assert!(data.len() <= MAX_CID_LEN);
        let mut bytes = [0u8; MAX_CID_LEN];
        let len = data.len().min(MAX_CID_LEN);
        bytes[..len].copy_from_slice(&data[..len]);
        Self {
            len: len as u8,
            bytes,
        }
    }

    pub fn generate(len: u8) -> Self {
        use rand::Rng;
        let len = len.min(MAX_CID_LEN as u8);
        let mut bytes = [0u8; MAX_CID_LEN];
        rand::rng().fill(&mut bytes[..len as usize]);
        Self { len, bytes }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl std::fmt::Debug for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CID(")?;
        for b in self.as_slice() {
            write!(f, "{:02x}", b)?;
        }
        write!(f, ")")
    }
}

/// Variable-length integer encoding (RFC 9000 §16).
///
/// Uses 1, 2, 4, or 8 bytes. The two MSBs of the first byte encode the length.
pub fn encode_varint(value: u64, buf: &mut Vec<u8>) -> Result<(), crate::error::QuicError> {
    if value > MAX_VARINT {
        return Err(crate::error::QuicError::InvalidFrame(format!(
            "varint value {value} exceeds QUIC maximum {MAX_VARINT}"
        )));
    }

    if value < 0x40 {
        buf.push(value as u8);
    } else if value < 0x4000 {
        buf.push(0x40 | (value >> 8) as u8);
        buf.push(value as u8);
    } else if value < 0x4000_0000 {
        buf.push(0x80 | (value >> 24) as u8);
        buf.push((value >> 16) as u8);
        buf.push((value >> 8) as u8);
        buf.push(value as u8);
    } else {
        buf.push(0xc0 | (value >> 56) as u8);
        buf.push((value >> 48) as u8);
        buf.push((value >> 40) as u8);
        buf.push((value >> 32) as u8);
        buf.push((value >> 24) as u8);
        buf.push((value >> 16) as u8);
        buf.push((value >> 8) as u8);
        buf.push(value as u8);
    }

    Ok(())
}

/// Decode a variable-length integer. Returns (value, bytes_consumed).
pub fn decode_varint(buf: &[u8]) -> Result<(u64, usize), crate::error::QuicError> {
    if buf.is_empty() {
        return Err(crate::error::QuicError::BufferTooSmall);
    }

    let first = buf[0];
    let len = 1 << (first >> 6);

    if buf.len() < len {
        return Err(crate::error::QuicError::BufferTooSmall);
    }

    let value = match len {
        1 => (first & 0x3f) as u64,
        2 => {
            let v = u16::from_be_bytes([first & 0x3f, buf[1]]);
            v as u64
        }
        4 => {
            let v = u32::from_be_bytes([first & 0x3f, buf[1], buf[2], buf[3]]);
            v as u64
        }
        8 => u64::from_be_bytes([
            first & 0x3f,
            buf[1],
            buf[2],
            buf[3],
            buf[4],
            buf[5],
            buf[6],
            buf[7],
        ]),
        _ => unreachable!(),
    };

    Ok((value, len))
}

/// Determine the byte length needed to encode a varint value.
pub fn varint_len(value: u64) -> usize {
    if value < 0x40 {
        1
    } else if value < 0x4000 {
        2
    } else if value < 0x4000_0000 {
        4
    } else {
        8
    }
}

/// Encode a packet number using the minimum number of bytes (1–4).
///
/// `pn` is the full packet number, `largest_acked` is the largest acknowledged
/// packet number (used to determine the minimum encoding length).
pub fn encode_packet_number(pn: u64, largest_acked: u64) -> (Vec<u8>, u8) {
    let num_unacked = pn.saturating_sub(largest_acked);

    let (encoded, len) = if num_unacked < 0x80 {
        (vec![pn as u8], 1u8)
    } else if num_unacked < 0x8000 {
        let v = (pn & 0xffff) as u16;
        (v.to_be_bytes().to_vec(), 2)
    } else if num_unacked < 0x800000 {
        let v = (pn & 0xffffff) as u32;
        (v.to_be_bytes()[1..].to_vec(), 3)
    } else {
        let v = (pn & 0xffffffff) as u32;
        (v.to_be_bytes().to_vec(), 4)
    };

    (encoded, len)
}

/// Decode a truncated packet number and reconstruct the full value.
///
/// `truncated_pn` is the decoded truncated value, `pn_len` is the number of
/// bytes it was encoded in, and `largest_pn` is the largest packet number
/// successfully processed.
pub fn decode_packet_number(truncated_pn: u64, pn_len: u8, largest_pn: u64) -> u64 {
    let pn_nbits = (pn_len as u64) * 8;
    let pn_win = 1u64 << pn_nbits;
    let pn_hwin = pn_win / 2;
    let pn_mask = pn_win - 1;

    // The expected packet number is one more than the largest received.
    let expected_pn = largest_pn + 1;
    let candidate = (expected_pn & !pn_mask) | truncated_pn;

    if candidate + pn_hwin <= expected_pn && candidate < (1u64 << 62) - pn_win {
        candidate + pn_win
    } else if candidate > expected_pn + pn_hwin && candidate >= pn_win {
        candidate - pn_win
    } else {
        candidate
    }
}

// ---- QUIC Transport Parameters (RFC 9000 §18) ----

/// Transport parameter IDs.
const TP_ORIGINAL_DESTINATION_CONNECTION_ID: u64 = 0x00;
const TP_MAX_IDLE_TIMEOUT: u64 = 0x01;
const TP_STATELESS_RESET_TOKEN: u64 = 0x02;
const TP_MAX_UDP_PAYLOAD_SIZE: u64 = 0x03;
const TP_INITIAL_MAX_DATA: u64 = 0x04;
const TP_INITIAL_MAX_STREAM_DATA_BIDI_LOCAL: u64 = 0x05;
const TP_INITIAL_MAX_STREAM_DATA_BIDI_REMOTE: u64 = 0x06;
const TP_INITIAL_MAX_STREAM_DATA_UNI: u64 = 0x07;
const TP_INITIAL_MAX_STREAMS_BIDI: u64 = 0x08;
const TP_INITIAL_MAX_STREAMS_UNI: u64 = 0x09;
const TP_ACK_DELAY_EXPONENT: u64 = 0x0a;
const TP_MAX_ACK_DELAY: u64 = 0x0b;
const TP_DISABLE_ACTIVE_MIGRATION: u64 = 0x0c;
const TP_PREFERRED_ADDRESS: u64 = 0x0d;
const TP_ACTIVE_CONNECTION_ID_LIMIT: u64 = 0x0e;
const TP_INITIAL_SOURCE_CONNECTION_ID: u64 = 0x0f;
const TP_RETRY_SOURCE_CONNECTION_ID: u64 = 0x10;
static NEXT_GREASE_TRANSPORT_PARAMETER_SEED: AtomicU64 = AtomicU64::new(0);

fn next_grease_transport_parameter() -> (u64, Vec<u8>) {
    let seed = NEXT_GREASE_TRANSPORT_PARAMETER_SEED.fetch_add(1, Ordering::Relaxed) + 1;
    let id = 31 * ((seed % 128) + 1) + 27;
    let value_len = ((seed >> 8) as usize % 8) + 1;
    let mut value = Vec::with_capacity(value_len);
    let mut x = seed ^ 0xfa_ce_b0_0c_13_37_aa_55;
    for _ in 0..value_len {
        value.push((x & 0xff) as u8);
        x = x.rotate_left(9) ^ 0x9e37_79b9_7f4a_7c15;
    }
    (id, value)
}

/// Default max UDP payload size when the peer omits the parameter (RFC 9000 §18.2).
pub const DEFAULT_MAX_UDP_PAYLOAD_SIZE: u64 = 65_527;
/// Minimum allowed max UDP payload size (RFC 9000 §18.2).
pub const MIN_MAX_UDP_PAYLOAD_SIZE: u64 = 1_200;
/// Default max ACK delay in milliseconds (RFC 9000 §18.2).
pub const DEFAULT_MAX_ACK_DELAY_MS: u64 = 25;
/// Maximum allowed max ACK delay in milliseconds (RFC 9000 §18.2).
pub const MAX_MAX_ACK_DELAY_MS: u64 = 1 << 14;

/// Local transport parameters we encode into the TLS handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTransportParameters {
    pub original_destination_connection_id: Option<ConnectionId>,
    pub max_idle_timeout_ms: u64,
    pub stateless_reset_token: Option<[u8; 16]>,
    pub max_udp_payload_size: u64,
    pub initial_max_data: u64,
    pub initial_max_stream_data_bidi_local: u64,
    pub initial_max_stream_data_bidi_remote: u64,
    pub initial_max_stream_data_uni: u64,
    pub initial_max_streams_bidi: u64,
    pub initial_max_streams_uni: u64,
    pub ack_delay_exponent: u32,
    pub max_ack_delay_ms: u64,
    pub disable_active_migration: bool,
    pub preferred_address: Option<Vec<u8>>,
    pub active_connection_id_limit: u64,
    pub initial_source_connection_id: Option<ConnectionId>,
    pub retry_source_connection_id: Option<ConnectionId>,
}

impl Default for LocalTransportParameters {
    fn default() -> Self {
        Self {
            original_destination_connection_id: None,
            max_idle_timeout_ms: 0,
            stateless_reset_token: None,
            max_udp_payload_size: DEFAULT_MAX_UDP_PAYLOAD_SIZE,
            initial_max_data: 0,
            initial_max_stream_data_bidi_local: 0,
            initial_max_stream_data_bidi_remote: 0,
            initial_max_stream_data_uni: 0,
            initial_max_streams_bidi: 0,
            initial_max_streams_uni: 0,
            ack_delay_exponent: 3,
            max_ack_delay_ms: DEFAULT_MAX_ACK_DELAY_MS,
            disable_active_migration: false,
            preferred_address: None,
            active_connection_id_limit: 2,
            initial_source_connection_id: None,
            retry_source_connection_id: None,
        }
    }
}

impl LocalTransportParameters {
    pub fn new(
        initial_max_data: u64,
        initial_max_stream_data: u64,
        initial_max_streams_bidi: u64,
        initial_max_streams_uni: u64,
    ) -> Self {
        Self {
            initial_max_data,
            initial_max_stream_data_bidi_local: initial_max_stream_data,
            initial_max_stream_data_bidi_remote: initial_max_stream_data,
            initial_max_stream_data_uni: initial_max_stream_data,
            initial_max_streams_bidi,
            initial_max_streams_uni,
            ..Self::default()
        }
    }
}

/// Parsed peer transport parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTransportParameters {
    pub original_destination_connection_id: Option<ConnectionId>,
    pub max_idle_timeout_ms: u64,
    pub max_udp_payload_size: u64,
    pub initial_max_data: u64,
    pub initial_max_stream_data_bidi_local: u64,
    pub initial_max_stream_data_bidi_remote: u64,
    pub initial_max_stream_data_uni: u64,
    pub initial_max_streams_bidi: u64,
    pub initial_max_streams_uni: u64,
    pub ack_delay_exponent: u32,
    pub max_ack_delay_ms: u64,
    pub disable_active_migration: bool,
    pub preferred_address: Option<Vec<u8>>,
    pub active_connection_id_limit: u64,
    pub initial_source_connection_id: Option<ConnectionId>,
    pub retry_source_connection_id: Option<ConnectionId>,
    /// Stateless reset token for the peer's initial CID (server-only).
    pub stateless_reset_token: Option<[u8; 16]>,
}

/// Decoded contents of the `preferred_address` transport parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPreferredAddress {
    pub ipv4: Option<SocketAddr>,
    pub ipv6: Option<SocketAddr>,
    pub connection_id: ConnectionId,
    pub stateless_reset_token: [u8; 16],
}

impl Default for PeerTransportParameters {
    fn default() -> Self {
        Self {
            original_destination_connection_id: None,
            max_idle_timeout_ms: 0,
            max_udp_payload_size: DEFAULT_MAX_UDP_PAYLOAD_SIZE,
            initial_max_data: 0,
            initial_max_stream_data_bidi_local: 0,
            initial_max_stream_data_bidi_remote: 0,
            initial_max_stream_data_uni: 0,
            initial_max_streams_bidi: 0,
            initial_max_streams_uni: 0,
            ack_delay_exponent: 3,
            max_ack_delay_ms: DEFAULT_MAX_ACK_DELAY_MS,
            disable_active_migration: false,
            preferred_address: None,
            active_connection_id_limit: 2,
            initial_source_connection_id: None,
            retry_source_connection_id: None,
            stateless_reset_token: None,
        }
    }
}

/// Encode a single transport parameter: varint(id) + varint(value_len) + varint(value).
fn encode_transport_param(
    id: u64,
    value: u64,
    buf: &mut Vec<u8>,
) -> Result<(), crate::error::QuicError> {
    encode_varint(id, buf)?;
    let value_len = varint_len(value) as u64;
    encode_varint(value_len, buf)?;
    encode_varint(value, buf)?;
    Ok(())
}

/// Encode a transport parameter whose value is an opaque byte string.
fn encode_transport_bytes_param(
    id: u64,
    value: &[u8],
    buf: &mut Vec<u8>,
) -> Result<(), crate::error::QuicError> {
    encode_varint(id, buf)?;
    encode_varint(value.len() as u64, buf)?;
    buf.extend_from_slice(value);
    Ok(())
}

fn transport_parameter_error(message: impl Into<String>) -> crate::error::QuicError {
    crate::error::QuicError::Transport(
        crate::error::TransportErrorCode::TransportParameterError,
        message.into(),
    )
}

fn decode_transport_param_value(value: &[u8]) -> Result<u64, crate::error::QuicError> {
    let (decoded, consumed) = decode_varint(value)
        .map_err(|_| transport_parameter_error("invalid transport parameter value"))?;
    if consumed != value.len() {
        return Err(transport_parameter_error(
            "transport parameter value has trailing bytes",
        ));
    }
    Ok(decoded)
}

fn decode_transport_connection_id(
    name: &str,
    value: &[u8],
) -> Result<ConnectionId, crate::error::QuicError> {
    if value.len() > MAX_CID_LEN {
        return Err(transport_parameter_error(format!(
            "{name} exceeds maximum connection ID length"
        )));
    }
    Ok(ConnectionId::from_slice(value))
}

fn validate_preferred_address(value: &[u8]) -> Result<(), crate::error::QuicError> {
    if value.len() < 41 {
        return Err(transport_parameter_error(
            "preferred_address is too short to contain the fixed fields",
        ));
    }

    let cid_len = value[24] as usize;
    if cid_len > MAX_CID_LEN {
        return Err(transport_parameter_error(
            "preferred_address connection ID exceeds maximum length",
        ));
    }

    let expected_len = 41 + cid_len;
    if value.len() != expected_len {
        return Err(transport_parameter_error(
            "preferred_address length does not match its encoded connection ID length",
        ));
    }

    Ok(())
}

pub(crate) fn decode_preferred_address(
    value: &[u8],
) -> Result<DecodedPreferredAddress, crate::error::QuicError> {
    validate_preferred_address(value)?;

    let ipv4_addr = Ipv4Addr::new(value[0], value[1], value[2], value[3]);
    let ipv4_port = u16::from_be_bytes([value[4], value[5]]);
    let ipv6_addr = Ipv6Addr::from(<[u8; 16]>::try_from(&value[6..22]).unwrap());
    let ipv6_port = u16::from_be_bytes([value[22], value[23]]);
    let cid_len = value[24] as usize;
    let cid = ConnectionId::from_slice(&value[25..25 + cid_len]);
    let mut stateless_reset_token = [0u8; 16];
    stateless_reset_token.copy_from_slice(&value[25 + cid_len..41 + cid_len]);

    let ipv4 = (ipv4_port != 0 && !ipv4_addr.is_unspecified())
        .then_some(SocketAddr::V4(SocketAddrV4::new(ipv4_addr, ipv4_port)));
    let ipv6 = (ipv6_port != 0 && !ipv6_addr.is_unspecified()).then_some(SocketAddr::V6(
        SocketAddrV6::new(ipv6_addr, ipv6_port, 0, 0),
    ));

    Ok(DecodedPreferredAddress {
        ipv4,
        ipv6,
        connection_id: cid,
        stateless_reset_token,
    })
}

/// Encode QUIC transport parameters for the handshake.
pub fn encode_transport_parameters(
    params: &LocalTransportParameters,
) -> Result<Vec<u8>, crate::error::QuicError> {
    let mut buf = Vec::with_capacity(96);

    if let Some(cid) = &params.original_destination_connection_id {
        encode_transport_bytes_param(
            TP_ORIGINAL_DESTINATION_CONNECTION_ID,
            cid.as_slice(),
            &mut buf,
        )?;
    }
    if params.max_idle_timeout_ms > 0 {
        encode_transport_param(TP_MAX_IDLE_TIMEOUT, params.max_idle_timeout_ms, &mut buf)?;
    }
    if let Some(token) = params.stateless_reset_token {
        encode_transport_bytes_param(TP_STATELESS_RESET_TOKEN, &token, &mut buf)?;
    }
    if params.max_udp_payload_size != DEFAULT_MAX_UDP_PAYLOAD_SIZE {
        encode_transport_param(
            TP_MAX_UDP_PAYLOAD_SIZE,
            params.max_udp_payload_size,
            &mut buf,
        )?;
    }
    encode_transport_param(TP_INITIAL_MAX_DATA, params.initial_max_data, &mut buf)?;
    encode_transport_param(
        TP_INITIAL_MAX_STREAM_DATA_BIDI_LOCAL,
        params.initial_max_stream_data_bidi_local,
        &mut buf,
    )?;
    encode_transport_param(
        TP_INITIAL_MAX_STREAM_DATA_BIDI_REMOTE,
        params.initial_max_stream_data_bidi_remote,
        &mut buf,
    )?;
    encode_transport_param(
        TP_INITIAL_MAX_STREAM_DATA_UNI,
        params.initial_max_stream_data_uni,
        &mut buf,
    )?;
    encode_transport_param(
        TP_INITIAL_MAX_STREAMS_BIDI,
        params.initial_max_streams_bidi,
        &mut buf,
    )?;
    encode_transport_param(
        TP_INITIAL_MAX_STREAMS_UNI,
        params.initial_max_streams_uni,
        &mut buf,
    )?;
    if params.ack_delay_exponent != 3 {
        encode_transport_param(
            TP_ACK_DELAY_EXPONENT,
            params.ack_delay_exponent as u64,
            &mut buf,
        )?;
    }
    if params.max_ack_delay_ms != DEFAULT_MAX_ACK_DELAY_MS {
        encode_transport_param(TP_MAX_ACK_DELAY, params.max_ack_delay_ms, &mut buf)?;
    }
    if params.disable_active_migration {
        encode_transport_bytes_param(TP_DISABLE_ACTIVE_MIGRATION, &[], &mut buf)?;
    }
    if let Some(preferred_address) = &params.preferred_address {
        validate_preferred_address(preferred_address)?;
        encode_transport_bytes_param(TP_PREFERRED_ADDRESS, preferred_address, &mut buf)?;
    }
    encode_transport_param(
        TP_ACTIVE_CONNECTION_ID_LIMIT,
        params.active_connection_id_limit,
        &mut buf,
    )?;
    if let Some(cid) = &params.initial_source_connection_id {
        encode_transport_bytes_param(TP_INITIAL_SOURCE_CONNECTION_ID, cid.as_slice(), &mut buf)?;
    }
    if let Some(cid) = &params.retry_source_connection_id {
        encode_transport_bytes_param(TP_RETRY_SOURCE_CONNECTION_ID, cid.as_slice(), &mut buf)?;
    }
    let (grease_id, grease_value) = next_grease_transport_parameter();
    encode_transport_bytes_param(grease_id, &grease_value, &mut buf)?;
    Ok(buf)
}

/// Parse peer transport parameters from the TLS handshake.
///
pub fn parse_transport_parameters(
    buf: &[u8],
) -> Result<PeerTransportParameters, crate::error::QuicError> {
    let mut offset = 0;
    let mut params = PeerTransportParameters::default();
    let mut seen = HashSet::new();

    while offset < buf.len() {
        let (id, n) = decode_varint(&buf[offset..])
            .map_err(|_| transport_parameter_error("truncated transport parameter id"))?;
        offset += n;

        let (len, n) = decode_varint(&buf[offset..])
            .map_err(|_| transport_parameter_error("truncated transport parameter length"))?;
        offset += n;

        let len = usize::try_from(len)
            .map_err(|_| transport_parameter_error("transport parameter length too large"))?;

        if buf.len() < offset + len {
            return Err(transport_parameter_error(
                "truncated transport parameter value",
            ));
        }

        let value = &buf[offset..offset + len];
        offset += len;

        if !seen.insert(id) {
            return Err(transport_parameter_error(format!(
                "duplicate transport parameter 0x{id:x}"
            )));
        }

        match id {
            TP_ORIGINAL_DESTINATION_CONNECTION_ID => {
                params.original_destination_connection_id = Some(decode_transport_connection_id(
                    "original_destination_connection_id",
                    value,
                )?);
            }
            TP_MAX_IDLE_TIMEOUT => {
                params.max_idle_timeout_ms = decode_transport_param_value(value)?;
            }
            TP_STATELESS_RESET_TOKEN => {
                if len != 16 {
                    return Err(transport_parameter_error(
                        "stateless reset token must be 16 bytes",
                    ));
                }

                let mut token = [0u8; 16];
                token.copy_from_slice(value);
                params.stateless_reset_token = Some(token);
            }
            TP_MAX_UDP_PAYLOAD_SIZE => {
                let size = decode_transport_param_value(value)?;
                if size < MIN_MAX_UDP_PAYLOAD_SIZE {
                    return Err(transport_parameter_error(
                        "max_udp_payload_size must be at least 1200",
                    ));
                }
                params.max_udp_payload_size = size;
            }
            TP_INITIAL_MAX_DATA => {
                params.initial_max_data = decode_transport_param_value(value)?;
            }
            TP_INITIAL_MAX_STREAM_DATA_BIDI_LOCAL => {
                params.initial_max_stream_data_bidi_local = decode_transport_param_value(value)?;
            }
            TP_INITIAL_MAX_STREAM_DATA_BIDI_REMOTE => {
                params.initial_max_stream_data_bidi_remote = decode_transport_param_value(value)?;
            }
            TP_INITIAL_MAX_STREAM_DATA_UNI => {
                params.initial_max_stream_data_uni = decode_transport_param_value(value)?;
            }
            TP_INITIAL_MAX_STREAMS_BIDI => {
                params.initial_max_streams_bidi = decode_transport_param_value(value)?;
            }
            TP_INITIAL_MAX_STREAMS_UNI => {
                params.initial_max_streams_uni = decode_transport_param_value(value)?;
            }
            TP_ACK_DELAY_EXPONENT => {
                let exponent = decode_transport_param_value(value)?;
                if exponent > MAX_ACK_DELAY_EXPONENT as u64 {
                    return Err(transport_parameter_error(
                        "ack delay exponent must be <= 20",
                    ));
                }
                params.ack_delay_exponent = exponent as u32;
            }
            TP_MAX_ACK_DELAY => {
                let delay = decode_transport_param_value(value)?;
                if delay > MAX_MAX_ACK_DELAY_MS {
                    return Err(transport_parameter_error(
                        "max_ack_delay must be <= 16384 milliseconds",
                    ));
                }
                params.max_ack_delay_ms = delay;
            }
            TP_DISABLE_ACTIVE_MIGRATION => {
                if !value.is_empty() {
                    return Err(transport_parameter_error(
                        "disable_active_migration must be zero-length",
                    ));
                }
                params.disable_active_migration = true;
            }
            TP_PREFERRED_ADDRESS => {
                validate_preferred_address(value)?;
                params.preferred_address = Some(value.to_vec());
            }
            TP_ACTIVE_CONNECTION_ID_LIMIT => {
                let limit = decode_transport_param_value(value)?;
                if limit < 2 {
                    return Err(transport_parameter_error(
                        "active_connection_id_limit must be >= 2",
                    ));
                }
                params.active_connection_id_limit = limit;
            }
            TP_INITIAL_SOURCE_CONNECTION_ID => {
                params.initial_source_connection_id = Some(decode_transport_connection_id(
                    "initial_source_connection_id",
                    value,
                )?);
            }
            TP_RETRY_SOURCE_CONNECTION_ID => {
                params.retry_source_connection_id = Some(decode_transport_connection_id(
                    "retry_source_connection_id",
                    value,
                )?);
            }
            _ => {}
        }
    }

    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_local_transport_parameters() -> LocalTransportParameters {
        let mut params = LocalTransportParameters::new(1024, 2048, 8, 4);
        params.max_idle_timeout_ms = 30_000;
        params.initial_source_connection_id = Some(ConnectionId::from_slice(&[1, 2, 3, 4]));
        params
    }

    #[test]
    fn varint_roundtrip() {
        let test_values = [
            0u64,
            1,
            63,
            64,
            16383,
            16384,
            0x3fff_ffff,
            0x4000_0000,
            u64::MAX >> 2,
        ];
        for &v in &test_values {
            let mut buf = Vec::new();
            encode_varint(v, &mut buf).unwrap();
            assert_eq!(buf.len(), varint_len(v));
            let (decoded, consumed) = decode_varint(&buf).unwrap();
            assert_eq!(decoded, v, "failed for value {v}");
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn varint_lengths() {
        assert_eq!(varint_len(0), 1);
        assert_eq!(varint_len(63), 1);
        assert_eq!(varint_len(64), 2);
        assert_eq!(varint_len(16383), 2);
        assert_eq!(varint_len(16384), 4);
        assert_eq!(varint_len(0x3fff_ffff), 4);
        assert_eq!(varint_len(0x4000_0000), 8);
    }

    #[test]
    fn connection_id_generate() {
        let cid = ConnectionId::generate(8);
        assert_eq!(cid.len(), 8);
        assert!(!cid.is_empty());
    }

    #[test]
    fn connection_id_from_slice() {
        let data = [1, 2, 3, 4];
        let cid = ConnectionId::from_slice(&data);
        assert_eq!(cid.as_slice(), &data);
        assert_eq!(cid.len(), 4);
    }

    #[test]
    fn connection_id_empty() {
        let cid = ConnectionId::empty();
        assert!(cid.is_empty());
        assert_eq!(cid.len(), 0);
    }

    #[test]
    fn packet_number_decode_basic() {
        // RFC 9000 Appendix A: sample packet number decoding
        assert_eq!(decode_packet_number(0xa82f, 2, 0xa82e), 0xa82f);
        assert_eq!(decode_packet_number(0x9b32, 2, 0xa82f), 0x9b32);
    }

    #[test]
    fn packet_number_encode_roundtrip() {
        let pn = 42u64;
        let largest_acked = 40u64;
        let (encoded, len) = encode_packet_number(pn, largest_acked);
        assert!((1..=4).contains(&len));

        let truncated = match len {
            1 => encoded[0] as u64,
            2 => u16::from_be_bytes([encoded[0], encoded[1]]) as u64,
            3 => {
                let mut b = [0u8; 4];
                b[1..].copy_from_slice(&encoded);
                u32::from_be_bytes(b) as u64
            }
            4 => u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as u64,
            _ => unreachable!(),
        };
        let decoded = decode_packet_number(truncated, len, largest_acked);
        assert_eq!(decoded, pn);
    }

    #[test]
    fn parse_transport_parameters_extracts_stateless_reset_token() {
        let token = [0xab; 16];
        let mut buf = Vec::new();
        encode_varint(TP_STATELESS_RESET_TOKEN, &mut buf).unwrap();
        encode_varint(token.len() as u64, &mut buf).unwrap();
        buf.extend_from_slice(&token);
        encode_transport_param(TP_INITIAL_MAX_DATA, 1024, &mut buf).unwrap();

        let params = parse_transport_parameters(&buf).unwrap();
        assert_eq!(params.stateless_reset_token, Some(token));
    }

    #[test]
    fn parse_transport_parameters_rejects_bad_stateless_reset_token_len() {
        let mut buf = Vec::new();
        encode_varint(TP_STATELESS_RESET_TOKEN, &mut buf).unwrap();
        encode_varint(15, &mut buf).unwrap();
        buf.extend_from_slice(&[0u8; 15]);

        let err = parse_transport_parameters(&buf).unwrap_err();
        assert!(matches!(
            err,
            crate::error::QuicError::Transport(
                crate::error::TransportErrorCode::TransportParameterError,
                _
            )
        ));
    }

    #[test]
    fn encode_transport_parameters_roundtrips_stateless_reset_token() {
        let token = [0x11; 16];
        let mut params = basic_local_transport_parameters();
        params.stateless_reset_token = Some(token);

        let encoded = encode_transport_parameters(&params).unwrap();
        let parsed = parse_transport_parameters(&encoded).unwrap();
        assert_eq!(parsed.stateless_reset_token, Some(token));
    }

    #[test]
    fn encode_transport_parameters_includes_varying_grease_parameter() {
        let encoded_one = encode_transport_parameters(&basic_local_transport_parameters()).unwrap();
        let encoded_two = encode_transport_parameters(&basic_local_transport_parameters()).unwrap();

        let mut grease_one = None;
        let mut cursor = encoded_one.as_slice();
        while !cursor.is_empty() {
            let (id, id_len) = decode_varint(cursor).unwrap();
            cursor = &cursor[id_len..];
            let (len, len_len) = decode_varint(cursor).unwrap();
            cursor = &cursor[len_len..];
            let len = len as usize;
            if id % 31 == 27 {
                grease_one = Some((id, cursor[..len].to_vec()));
            }
            cursor = &cursor[len..];
        }

        let mut grease_two = None;
        let mut cursor = encoded_two.as_slice();
        while !cursor.is_empty() {
            let (id, id_len) = decode_varint(cursor).unwrap();
            cursor = &cursor[id_len..];
            let (len, len_len) = decode_varint(cursor).unwrap();
            cursor = &cursor[len_len..];
            let len = len as usize;
            if id % 31 == 27 {
                grease_two = Some((id, cursor[..len].to_vec()));
            }
            cursor = &cursor[len..];
        }

        let grease_one =
            grease_one.expect("GREASE transport parameter missing from first encoding");
        let grease_two =
            grease_two.expect("GREASE transport parameter missing from second encoding");
        assert_ne!(grease_one, grease_two, "GREASE should vary across encodes");
    }

    #[test]
    fn parse_transport_parameters_ignores_reserved_grease_parameter() {
        let mut buf = Vec::new();
        encode_transport_param(TP_INITIAL_MAX_DATA, 1024, &mut buf).unwrap();
        encode_transport_bytes_param(31 * 7 + 27, &[0xde, 0xad, 0xbe, 0xef], &mut buf).unwrap();

        let params = parse_transport_parameters(&buf).unwrap();
        assert_eq!(params.initial_max_data, 1024);
    }

    #[test]
    fn parse_transport_parameters_extracts_flow_control_and_limits() {
        let mut params = basic_local_transport_parameters();
        params.ack_delay_exponent = 5;
        params.max_ack_delay_ms = 42;
        params.max_udp_payload_size = 1400;
        params.disable_active_migration = true;
        params.active_connection_id_limit = 6;
        params.original_destination_connection_id = Some(ConnectionId::from_slice(&[9, 8, 7]));
        params.retry_source_connection_id = Some(ConnectionId::from_slice(&[6, 5, 4]));

        let preferred_address = {
            let mut bytes = vec![0u8; 25];
            bytes[24] = 4;
            bytes.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
            bytes.extend_from_slice(&[0x11; 16]);
            bytes
        };
        params.preferred_address = Some(preferred_address.clone());

        let encoded = encode_transport_parameters(&params).unwrap();
        let parsed = parse_transport_parameters(&encoded).unwrap();

        assert_eq!(
            parsed.original_destination_connection_id,
            Some(ConnectionId::from_slice(&[9, 8, 7]))
        );
        assert_eq!(parsed.max_idle_timeout_ms, 30_000);
        assert_eq!(parsed.max_udp_payload_size, 1400);
        assert_eq!(parsed.initial_max_data, 1024);
        assert_eq!(parsed.initial_max_stream_data_bidi_local, 2048);
        assert_eq!(parsed.initial_max_stream_data_bidi_remote, 2048);
        assert_eq!(parsed.initial_max_stream_data_uni, 2048);
        assert_eq!(parsed.initial_max_streams_bidi, 8);
        assert_eq!(parsed.initial_max_streams_uni, 4);
        assert_eq!(parsed.ack_delay_exponent, 5);
        assert_eq!(parsed.max_ack_delay_ms, 42);
        assert!(parsed.disable_active_migration);
        assert_eq!(parsed.preferred_address, Some(preferred_address));
        assert_eq!(parsed.active_connection_id_limit, 6);
        assert_eq!(
            parsed.initial_source_connection_id,
            Some(ConnectionId::from_slice(&[1, 2, 3, 4]))
        );
        assert_eq!(
            parsed.retry_source_connection_id,
            Some(ConnectionId::from_slice(&[6, 5, 4]))
        );
    }

    #[test]
    fn parse_transport_parameters_rejects_small_active_connection_id_limit() {
        let mut buf = Vec::new();
        encode_transport_param(TP_ACTIVE_CONNECTION_ID_LIMIT, 1, &mut buf).unwrap();

        let err = parse_transport_parameters(&buf).unwrap_err();
        assert!(matches!(
            err,
            crate::error::QuicError::Transport(
                crate::error::TransportErrorCode::TransportParameterError,
                _
            )
        ));
    }

    #[test]
    fn parse_transport_parameters_rejects_duplicate_parameter() {
        let mut buf = Vec::new();
        encode_transport_param(TP_INITIAL_MAX_DATA, 1024, &mut buf).unwrap();
        encode_transport_param(TP_INITIAL_MAX_DATA, 2048, &mut buf).unwrap();

        let err = parse_transport_parameters(&buf).unwrap_err();
        assert!(matches!(
            err,
            crate::error::QuicError::Transport(
                crate::error::TransportErrorCode::TransportParameterError,
                _
            )
        ));
    }

    #[test]
    fn parse_transport_parameters_rejects_small_max_udp_payload_size() {
        let mut buf = Vec::new();
        encode_transport_param(TP_MAX_UDP_PAYLOAD_SIZE, 1199, &mut buf).unwrap();

        let err = parse_transport_parameters(&buf).unwrap_err();
        assert!(matches!(
            err,
            crate::error::QuicError::Transport(
                crate::error::TransportErrorCode::TransportParameterError,
                _
            )
        ));
    }

    #[test]
    fn parse_transport_parameters_rejects_large_max_ack_delay() {
        let mut buf = Vec::new();
        encode_transport_param(TP_MAX_ACK_DELAY, MAX_MAX_ACK_DELAY_MS + 1, &mut buf).unwrap();

        let err = parse_transport_parameters(&buf).unwrap_err();
        assert!(matches!(
            err,
            crate::error::QuicError::Transport(
                crate::error::TransportErrorCode::TransportParameterError,
                _
            )
        ));
    }

    #[test]
    fn parse_transport_parameters_rejects_malformed_preferred_address() {
        let mut buf = Vec::new();
        let mut preferred_address = vec![0u8; 25];
        preferred_address[24] = 6;
        preferred_address.extend_from_slice(&[0xaa; 4]);
        preferred_address.extend_from_slice(&[0x11; 16]);
        encode_transport_bytes_param(TP_PREFERRED_ADDRESS, &preferred_address, &mut buf).unwrap();

        let err = parse_transport_parameters(&buf).unwrap_err();
        assert!(matches!(
            err,
            crate::error::QuicError::Transport(
                crate::error::TransportErrorCode::TransportParameterError,
                _
            )
        ));
    }

    #[test]
    fn decode_preferred_address_extracts_socket_addresses_and_token() {
        let mut preferred_address = Vec::new();
        preferred_address.extend_from_slice(&[192, 0, 2, 10]);
        preferred_address.extend_from_slice(&4433u16.to_be_bytes());
        preferred_address.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        preferred_address.extend_from_slice(&8443u16.to_be_bytes());
        preferred_address.push(4);
        preferred_address.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        preferred_address.extend_from_slice(&[0x11; 16]);

        let decoded = decode_preferred_address(&preferred_address).unwrap();

        assert_eq!(
            decoded.ipv4,
            Some("192.0.2.10:4433".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(
            decoded.ipv6,
            Some("[::1]:8443".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(
            decoded.connection_id,
            ConnectionId::from_slice(&[0xaa, 0xbb, 0xcc, 0xdd])
        );
        assert_eq!(decoded.stateless_reset_token, [0x11; 16]);
    }

    #[test]
    fn encode_varint_rejects_values_above_quic_limit() {
        let mut buf = Vec::new();
        let err = encode_varint(1u64 << 62, &mut buf).unwrap_err();
        assert!(matches!(err, crate::error::QuicError::InvalidFrame(_)));
        assert!(buf.is_empty());
    }
}
