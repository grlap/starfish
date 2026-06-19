//! Retry packet type (RFC 9000 §17.2.5).
//!
//! Retry packets are sent by the server to validate the client's address.
//! They carry a token and a Retry Integrity Tag (RFC 9001 §5.8) but no
//! packet number or encrypted payload.
//!
//! ```text
//! Retry Packet {
//!   Header Form (1) = 1,
//!   Fixed Bit (1) = 1,
//!   Long Packet Type (2) = 3,
//!   Unused (4),
//!   Version (32),
//!   DCID Len (8), DCID (0..160),
//!   SCID Len (8), SCID (0..160),
//!   Retry Token (..),
//!   Retry Integrity Tag (128),
//! }
//! ```

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use ring::aead;
use ring::hkdf;
use ring::hmac;

use super::{ConnectionId, MAX_CID_LEN, QUIC_VERSION_1};
use crate::error::QuicError;

/// Size of the Retry Integrity Tag (AES-128-GCM tag).
pub const RETRY_INTEGRITY_TAG_LEN: usize = 16;

/// The fixed key used to compute Retry Integrity Tags (RFC 9001 §5.8).
pub const RETRY_INTEGRITY_KEY: [u8; 16] = [
    0xbe, 0x0c, 0x69, 0x0b, 0x9f, 0x66, 0x57, 0x5a, 0x1d, 0x76, 0x6b, 0x54, 0xe3, 0x68, 0xc8, 0x4e,
];

/// The fixed nonce used to compute Retry Integrity Tags (RFC 9001 §5.8).
pub const RETRY_INTEGRITY_NONCE: [u8; 12] = [
    0x46, 0x15, 0x99, 0xd3, 0x5d, 0x63, 0x2b, 0xf2, 0x23, 0x98, 0x25, 0xbb,
];

/// Parsed Retry packet fields.
#[derive(Debug, Clone)]
pub struct RetryPacket {
    #[allow(dead_code)]
    pub dcid: ConnectionId,
    pub scid: ConnectionId,
    pub token: Vec<u8>,
    #[allow(dead_code)]
    pub integrity_tag: [u8; RETRY_INTEGRITY_TAG_LEN],
}

/// Parse a Retry packet from the buffer starting after the long header.
///
/// `header_len` is where the retry token starts. The last 16 bytes are the
/// integrity tag.
pub fn parse_retry_fields(
    buf: &[u8],
    header_len: usize,
    dcid: ConnectionId,
    scid: ConnectionId,
) -> Result<RetryPacket, QuicError> {
    if buf.len() < header_len + RETRY_INTEGRITY_TAG_LEN {
        return Err(QuicError::InvalidPacket(
            "retry packet too short for integrity tag".into(),
        ));
    }

    let tag_start = buf.len() - RETRY_INTEGRITY_TAG_LEN;
    let token = buf[header_len..tag_start].to_vec();
    let mut integrity_tag = [0u8; RETRY_INTEGRITY_TAG_LEN];
    integrity_tag.copy_from_slice(&buf[tag_start..]);

    Ok(RetryPacket {
        dcid,
        scid,
        token,
        integrity_tag,
    })
}

/// Encode a Retry packet (without the integrity tag — caller must compute
/// and append it).
pub fn encode_retry_packet(dcid: &ConnectionId, scid: &ConnectionId, token: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32 + token.len() + RETRY_INTEGRITY_TAG_LEN);

    // First byte: form=1, fixed=1, type=Retry(0x03), unused=0
    buf.push(0xf0);

    // Version
    buf.extend_from_slice(&QUIC_VERSION_1.to_be_bytes());

    // DCID
    buf.push(dcid.len() as u8);
    buf.extend_from_slice(dcid.as_slice());

    // SCID
    buf.push(scid.len() as u8);
    buf.extend_from_slice(scid.as_slice());

    // Token
    buf.extend_from_slice(token);

    buf
}

/// Build the Retry Pseudo-Packet used as AAD for the integrity tag (RFC 9001 §5.8).
///
/// The pseudo-packet prepends the original DCID (from the client's Initial).
pub fn build_retry_pseudo_packet(original_dcid: &ConnectionId, retry_packet: &[u8]) -> Vec<u8> {
    let mut pseudo = Vec::with_capacity(1 + original_dcid.len() + retry_packet.len());
    pseudo.push(original_dcid.len() as u8);
    pseudo.extend_from_slice(original_dcid.as_slice());
    pseudo.extend_from_slice(retry_packet);
    pseudo
}

/// Compute the Retry Integrity Tag (RFC 9001 §5.8).
///
/// Uses AES-128-GCM with the fixed key/nonce to authenticate the Retry packet
/// bound to the original DCID.
pub fn compute_retry_integrity_tag(
    original_dcid: &ConnectionId,
    retry_packet_without_tag: &[u8],
) -> Result<[u8; RETRY_INTEGRITY_TAG_LEN], QuicError> {
    let pseudo = build_retry_pseudo_packet(original_dcid, retry_packet_without_tag);

    let key = aead::UnboundKey::new(&aead::AES_128_GCM, &RETRY_INTEGRITY_KEY)
        .map_err(|_| QuicError::InvalidPacket("retry integrity key error".into()))?;
    let key = aead::LessSafeKey::new(key);

    let nonce = aead::Nonce::assume_unique_for_key(RETRY_INTEGRITY_NONCE);

    // Encrypt empty plaintext — the tag authenticates the AAD (pseudo-packet).
    let mut in_out = Vec::new();
    key.seal_in_place_append_tag(nonce, aead::Aad::from(pseudo), &mut in_out)
        .map_err(|_| QuicError::InvalidPacket("retry integrity tag computation failed".into()))?;

    let mut tag = [0u8; RETRY_INTEGRITY_TAG_LEN];
    tag.copy_from_slice(&in_out);
    Ok(tag)
}

/// Verify the Retry Integrity Tag on a received Retry packet.
///
/// `original_dcid` is the DCID the client originally sent in its first Initial.
/// `retry_packet_with_tag` is the entire Retry packet including the 16-byte tag.
pub fn verify_retry_integrity_tag(
    original_dcid: &ConnectionId,
    retry_packet_with_tag: &[u8],
) -> Result<bool, QuicError> {
    if retry_packet_with_tag.len() < RETRY_INTEGRITY_TAG_LEN {
        return Ok(false);
    }

    let tag_start = retry_packet_with_tag.len() - RETRY_INTEGRITY_TAG_LEN;
    let retry_without_tag = &retry_packet_with_tag[..tag_start];
    let received_tag = &retry_packet_with_tag[tag_start..];

    let computed_tag = compute_retry_integrity_tag(original_dcid, retry_without_tag)?;

    // The Retry integrity key is a fixed public constant (RFC 9001 §5.8),
    // so constant-time comparison is not needed for security here.
    Ok(received_tag == computed_tag.as_slice())
}

// ---- Server-side token generation/validation ----

/// Maximum age for a Retry token (seconds).
const RETRY_TOKEN_MAX_AGE_SECS: u64 = 10;
/// Maximum age for a NEW_TOKEN token (seconds).
const NEW_TOKEN_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;
const TOKEN_KIND_RETRY: u8 = 0x00;
const TOKEN_KIND_NEW_TOKEN: u8 = 0x01;
const TOKEN_IP_VERSION_V4: u8 = 0x04;
const TOKEN_IP_VERSION_V6: u8 = 0x06;
const TOKEN_TAG_LEN: usize = 16;
const NEW_TOKEN_NONCE_LEN: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryTokenValidation {
    Valid(ConnectionId),
    Invalid,
    Unknown,
}

fn client_ip_version(client_addr: &SocketAddr) -> u8 {
    match client_addr.ip() {
        std::net::IpAddr::V4(_) => TOKEN_IP_VERSION_V4,
        std::net::IpAddr::V6(_) => TOKEN_IP_VERSION_V6,
    }
}

fn client_ip_bytes(client_addr: &SocketAddr) -> Vec<u8> {
    match client_addr.ip() {
        std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
        std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
    }
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct TokenKeyLen;

impl hkdf::KeyType for TokenKeyLen {
    fn len(&self) -> usize {
        32
    }
}

/// Derive a purpose-specific token subkey from the server secret via HKDF.
///
/// Key-separation invariant: the Retry token (HMAC-SHA256) and NEW_TOKEN
/// (AES-256-GCM) keys MUST come from distinct HKDF labels so the same raw
/// `retry_secret` is never reused across two different cryptographic primitives.
/// Do not collapse `retry-hmac` and `new-token-aead` into one key.
fn derive_token_subkey(secret: &[u8; 32], label: &[u8]) -> [u8; 32] {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"starfish-quic-token-v1");
    let prk = salt.extract(secret);
    let info = [label];
    let okm = prk
        .expand(&info, TokenKeyLen)
        .expect("fixed label and 32-byte output are valid for HKDF");
    let mut key = [0u8; 32];
    okm.fill(&mut key)
        .expect("32-byte HKDF output should fill the destination");
    key
}

fn derive_retry_token_key(secret: &[u8; 32]) -> [u8; 32] {
    derive_token_subkey(secret, b"retry-hmac")
}

fn derive_new_token_key(secret: &[u8; 32]) -> [u8; 32] {
    derive_token_subkey(secret, b"new-token-aead")
}

/// Encode a stateless Retry token for address validation.
///
/// Token format: `[kind(1)] [ip_version(1)] [addr_bytes] [port(2)] [timestamp(8)] [original_dcid_len(1)] [original_dcid] [hmac_tag(16)]`.
pub fn encode_retry_token(
    client_addr: &SocketAddr,
    original_dcid: &ConnectionId,
    retry_secret: &[u8; 32],
) -> Vec<u8> {
    let mut token = Vec::with_capacity(66);

    token.push(TOKEN_KIND_RETRY);
    token.push(client_ip_version(client_addr));
    token.extend_from_slice(&client_ip_bytes(client_addr));
    token.extend_from_slice(&client_addr.port().to_be_bytes());
    token.extend_from_slice(&current_time_ms().to_be_bytes());
    token.push(original_dcid.len() as u8);
    token.extend_from_slice(original_dcid.as_slice());

    let retry_key = derive_retry_token_key(retry_secret);
    let key = hmac::Key::new(hmac::HMAC_SHA256, &retry_key);
    let tag = hmac::sign(&key, &token);
    token.extend_from_slice(&tag.as_ref()[..TOKEN_TAG_LEN]);

    token
}

/// Validate a Retry token and extract the original DCID.
///
/// Tokens that cannot be authenticated are returned as [`RetryTokenValidation::Unknown`],
/// allowing the caller to treat them like an absent token. Tokens that authenticate
/// successfully but fail address or freshness checks are returned as
/// [`RetryTokenValidation::Invalid`].
pub fn validate_retry_token(
    token: &[u8],
    client_addr: &SocketAddr,
    retry_secret: &[u8; 32],
) -> Result<RetryTokenValidation, QuicError> {
    let addr_len = match client_addr.ip() {
        std::net::IpAddr::V4(_) => 4usize,
        std::net::IpAddr::V6(_) => 16usize,
    };
    let min_len = 1 + 1 + addr_len + 2 + 8 + 1 + TOKEN_TAG_LEN;
    if token.len() < min_len || token.first() != Some(&TOKEN_KIND_RETRY) {
        return Ok(RetryTokenValidation::Unknown);
    }

    let tag_start = token.len() - TOKEN_TAG_LEN;
    let data = &token[..tag_start];
    let received_tag = &token[tag_start..];

    let retry_key = derive_retry_token_key(retry_secret);
    let key = hmac::Key::new(hmac::HMAC_SHA256, &retry_key);
    let computed_tag = hmac::sign(&key, data);
    #[allow(deprecated)]
    let tag_matches = ring::constant_time::verify_slices_are_equal(
        received_tag,
        &computed_tag.as_ref()[..TOKEN_TAG_LEN],
    )
    .is_ok();
    if !tag_matches {
        return Ok(RetryTokenValidation::Unknown);
    }

    let expected_ip_version = client_ip_version(client_addr);
    if data.get(1).copied() != Some(expected_ip_version) {
        return Ok(RetryTokenValidation::Invalid);
    }

    let mut offset = 2;
    let expected_addr_bytes = client_ip_bytes(client_addr);
    if data.len() < offset + addr_len || data[offset..offset + addr_len] != expected_addr_bytes {
        return Ok(RetryTokenValidation::Invalid);
    }
    offset += addr_len;

    if data.len() < offset + 2 {
        return Ok(RetryTokenValidation::Invalid);
    }
    let token_port = u16::from_be_bytes([data[offset], data[offset + 1]]);
    if token_port != client_addr.port() {
        return Ok(RetryTokenValidation::Invalid);
    }
    offset += 2;

    if data.len() < offset + 8 {
        return Ok(RetryTokenValidation::Invalid);
    }
    let token_timestamp_ms = u64::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]);
    offset += 8;

    let now_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(_) => return Ok(RetryTokenValidation::Invalid),
    };
    if now_ms < token_timestamp_ms || now_ms - token_timestamp_ms > RETRY_TOKEN_MAX_AGE_SECS * 1000
    {
        return Ok(RetryTokenValidation::Invalid);
    }

    let Some(&dcid_len) = data.get(offset) else {
        return Ok(RetryTokenValidation::Invalid);
    };
    let dcid_len = dcid_len as usize;
    if dcid_len > MAX_CID_LEN {
        return Ok(RetryTokenValidation::Invalid);
    }
    offset += 1;
    if data.len() < offset + dcid_len {
        return Ok(RetryTokenValidation::Invalid);
    }

    Ok(RetryTokenValidation::Valid(ConnectionId::from_slice(
        &data[offset..offset + dcid_len],
    )))
}

/// Encode a stateless NEW_TOKEN token for future address validation.
///
/// The client IP address is encrypted rather than carried in cleartext so the token does not
/// expose client-address information when replayed on a later connection.
pub fn encode_new_token(client_addr: &SocketAddr, token_secret: &[u8; 32]) -> Vec<u8> {
    use rand::Rng;

    let mut nonce_bytes = [0u8; NEW_TOKEN_NONCE_LEN];
    rand::rng().fill(&mut nonce_bytes);

    let mut token = Vec::with_capacity(1 + NEW_TOKEN_NONCE_LEN + 32);
    token.push(TOKEN_KIND_NEW_TOKEN);
    token.extend_from_slice(&nonce_bytes);

    let ip_bytes = client_ip_bytes(client_addr);
    let mut plaintext = Vec::with_capacity(1 + ip_bytes.len() + 8);
    plaintext.push(client_ip_version(client_addr));
    plaintext.extend_from_slice(&ip_bytes);
    plaintext.extend_from_slice(&current_time_ms().to_be_bytes());

    let new_token_key = derive_new_token_key(token_secret);
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &new_token_key)
        .expect("AES-256-GCM accepts 32-byte NEW_TOKEN secrets");
    let key = aead::LessSafeKey::new(key);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);

    key.seal_in_place_append_tag(
        nonce,
        aead::Aad::from([TOKEN_KIND_NEW_TOKEN]),
        &mut plaintext,
    )
    .expect("NEW_TOKEN sealing should succeed");
    token.extend_from_slice(&plaintext);
    token
}

/// Validate a NEW_TOKEN token for the client's current IP address.
pub fn validate_new_token(
    token: &[u8],
    client_addr: &SocketAddr,
    token_secret: &[u8; 32],
) -> Result<bool, QuicError> {
    let addr_len = match client_addr.ip() {
        std::net::IpAddr::V4(_) => 4usize,
        std::net::IpAddr::V6(_) => 16usize,
    };
    let expected_len = 1 + NEW_TOKEN_NONCE_LEN + 1 + addr_len + 8 + TOKEN_TAG_LEN;
    if token.len() != expected_len || token.first() != Some(&TOKEN_KIND_NEW_TOKEN) {
        return Ok(false);
    }

    let mut nonce_bytes = [0u8; NEW_TOKEN_NONCE_LEN];
    nonce_bytes.copy_from_slice(&token[1..1 + NEW_TOKEN_NONCE_LEN]);
    let mut ciphertext = token[1 + NEW_TOKEN_NONCE_LEN..].to_vec();

    let new_token_key = derive_new_token_key(token_secret);
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &new_token_key)
        .map_err(|_| QuicError::InvalidPacket("new token key error".into()))?;
    let key = aead::LessSafeKey::new(key);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let plaintext = match key.open_in_place(
        nonce,
        aead::Aad::from([TOKEN_KIND_NEW_TOKEN]),
        &mut ciphertext,
    ) {
        Ok(plaintext) => plaintext,
        Err(_) => return Ok(false),
    };

    if plaintext.len() != 1 + addr_len + 8 || plaintext[0] != client_ip_version(client_addr) {
        return Ok(false);
    }

    let expected_addr_bytes = client_ip_bytes(client_addr);
    if plaintext[1..1 + addr_len] != expected_addr_bytes {
        return Ok(false);
    }

    let timestamp_offset = 1 + addr_len;
    let token_timestamp_ms = u64::from_be_bytes([
        plaintext[timestamp_offset],
        plaintext[timestamp_offset + 1],
        plaintext[timestamp_offset + 2],
        plaintext[timestamp_offset + 3],
        plaintext[timestamp_offset + 4],
        plaintext[timestamp_offset + 5],
        plaintext[timestamp_offset + 6],
        plaintext[timestamp_offset + 7],
    ]);

    let now_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(_) => return Ok(false),
    };
    if now_ms < token_timestamp_ms || now_ms - token_timestamp_ms > NEW_TOKEN_MAX_AGE_SECS * 1000 {
        return Ok(false);
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_retry_structure() {
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let scid = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let token = b"retry_token_value";

        let pkt = encode_retry_packet(&dcid, &scid, token);

        // First byte should indicate Retry
        assert_eq!(pkt[0] & 0xf0, 0xf0);

        // Version should be QUIC v1
        assert_eq!(
            u32::from_be_bytes([pkt[1], pkt[2], pkt[3], pkt[4]]),
            QUIC_VERSION_1
        );
    }

    #[test]
    fn parse_retry_roundtrip() {
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let scid = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let token = b"test_token";

        let mut pkt = encode_retry_packet(&dcid, &scid, token);
        // Append a fake integrity tag
        pkt.extend_from_slice(&[0xaa; RETRY_INTEGRITY_TAG_LEN]);

        // header_len = 1 + 4 + 1 + 4 + 1 + 4 = 15
        let header_len = 1 + 4 + 1 + dcid.len() + 1 + scid.len();
        let retry = parse_retry_fields(&pkt, header_len, dcid, scid).unwrap();
        assert_eq!(retry.token, token);
        assert_eq!(retry.integrity_tag, [0xaa; RETRY_INTEGRITY_TAG_LEN]);
    }

    #[test]
    fn pseudo_packet_includes_original_dcid() {
        let original_dcid = ConnectionId::from_slice(&[0xde, 0xad]);
        let retry_data = vec![0x01, 0x02, 0x03];

        let pseudo = build_retry_pseudo_packet(&original_dcid, &retry_data);
        assert_eq!(pseudo[0], 2); // original dcid length
        assert_eq!(&pseudo[1..3], original_dcid.as_slice());
        assert_eq!(&pseudo[3..], &retry_data);
    }

    #[test]
    fn integrity_tag_roundtrip() {
        let original_dcid = ConnectionId::from_slice(&[0x83, 0x94, 0xc8, 0xf0]);
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let scid = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let token = b"test_retry_token";

        // Build retry packet without tag
        let retry_pkt = encode_retry_packet(&dcid, &scid, token);

        // Compute integrity tag
        let tag = compute_retry_integrity_tag(&original_dcid, &retry_pkt).unwrap();

        // Append tag to make full packet
        let mut full_pkt = retry_pkt;
        full_pkt.extend_from_slice(&tag);

        // Verify
        assert!(verify_retry_integrity_tag(&original_dcid, &full_pkt).unwrap());
    }

    #[test]
    fn integrity_tag_rejects_tampered() {
        let original_dcid = ConnectionId::from_slice(&[0x83, 0x94, 0xc8, 0xf0]);
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let scid = ConnectionId::from_slice(&[5, 6, 7, 8]);

        let retry_pkt = encode_retry_packet(&dcid, &scid, b"token");
        let tag = compute_retry_integrity_tag(&original_dcid, &retry_pkt).unwrap();

        let mut full_pkt = retry_pkt;
        full_pkt.extend_from_slice(&tag);

        // Tamper with the packet
        full_pkt[10] ^= 0xff;
        assert!(!verify_retry_integrity_tag(&original_dcid, &full_pkt).unwrap());
    }

    #[test]
    fn integrity_tag_rejects_wrong_dcid() {
        let original_dcid = ConnectionId::from_slice(&[0x83, 0x94]);
        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let scid = ConnectionId::from_slice(&[5, 6, 7, 8]);

        let retry_pkt = encode_retry_packet(&dcid, &scid, b"token");
        let tag = compute_retry_integrity_tag(&original_dcid, &retry_pkt).unwrap();

        let mut full_pkt = retry_pkt;
        full_pkt.extend_from_slice(&tag);

        // Verify with a different original DCID
        let wrong_dcid = ConnectionId::from_slice(&[0xaa, 0xbb]);
        assert!(!verify_retry_integrity_tag(&wrong_dcid, &full_pkt).unwrap());
    }

    #[test]
    fn retry_token_encode_validate_roundtrip() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let original_dcid = ConnectionId::from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let secret = [0x42u8; 32];

        let token = encode_retry_token(&addr, &original_dcid, &secret);

        let result = validate_retry_token(&token, &addr, &secret).unwrap();
        assert!(matches!(
            result,
            RetryTokenValidation::Valid(ref dcid) if dcid.as_slice() == original_dcid.as_slice()
        ));
    }

    #[test]
    fn retry_token_rejects_wrong_addr() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let wrong_addr: SocketAddr = "192.168.1.1:12345".parse().unwrap();
        let original_dcid = ConnectionId::from_slice(&[0xde, 0xad]);
        let secret = [0x42u8; 32];

        let token = encode_retry_token(&addr, &original_dcid, &secret);

        let result = validate_retry_token(&token, &wrong_addr, &secret).unwrap();
        assert_eq!(result, RetryTokenValidation::Invalid);
    }

    #[test]
    fn retry_token_rejects_wrong_secret() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let original_dcid = ConnectionId::from_slice(&[0xde, 0xad]);
        let secret = [0x42u8; 32];
        let wrong_secret = [0x99u8; 32];

        let token = encode_retry_token(&addr, &original_dcid, &secret);

        let result = validate_retry_token(&token, &addr, &wrong_secret).unwrap();
        assert_eq!(result, RetryTokenValidation::Unknown);
    }

    #[test]
    fn retry_token_ipv6_roundtrip() {
        let addr: SocketAddr = "[::1]:9999".parse().unwrap();
        let original_dcid = ConnectionId::from_slice(&[0xca, 0xfe]);
        let secret = [0x55u8; 32];

        let token = encode_retry_token(&addr, &original_dcid, &secret);

        let result = validate_retry_token(&token, &addr, &secret).unwrap();
        assert!(matches!(
            result,
            RetryTokenValidation::Valid(ref dcid) if dcid.as_slice() == original_dcid.as_slice()
        ));
    }

    #[test]
    fn retry_token_ipv6_rejects_ipv4() {
        let addr_v6: SocketAddr = "[::1]:9999".parse().unwrap();
        let addr_v4: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let original_dcid = ConnectionId::from_slice(&[0xca, 0xfe]);
        let secret = [0x55u8; 32];

        let token = encode_retry_token(&addr_v6, &original_dcid, &secret);

        let result = validate_retry_token(&token, &addr_v4, &secret).unwrap();
        assert_eq!(result, RetryTokenValidation::Invalid);
    }

    #[test]
    fn retry_token_rejects_truncated() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let secret = [0x42u8; 32];

        let result = validate_retry_token(&[0u8; 10], &addr, &secret).unwrap();
        assert_eq!(result, RetryTokenValidation::Unknown);

        let addr_v6: SocketAddr = "[::1]:9999".parse().unwrap();
        let result = validate_retry_token(&[0u8; 35], &addr_v6, &secret).unwrap();
        assert_eq!(result, RetryTokenValidation::Unknown);
    }

    #[test]
    fn retry_token_rejects_tampered_payload() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let secret = [0x42u8; 32];
        let mut token =
            encode_retry_token(&addr, &ConnectionId::from_slice(&[1, 2, 3, 4]), &secret);

        let payload_len = token.len() - TOKEN_TAG_LEN;
        let dcid_len_index = payload_len - 5;
        token[dcid_len_index] = (MAX_CID_LEN as u8) + 1;

        let result = validate_retry_token(&token, &addr, &secret).unwrap();
        assert_eq!(result, RetryTokenValidation::Unknown);
    }

    #[test]
    fn retry_validation_ignores_new_token_format() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let secret = [0x42u8; 32];
        let token = encode_new_token(&addr, &secret);

        let result = validate_retry_token(&token, &addr, &secret).unwrap();
        assert_eq!(result, RetryTokenValidation::Unknown);
    }

    #[test]
    fn token_key_derivation_separates_retry_and_new_token_keys() {
        let secret = [0x24u8; 32];

        assert_ne!(
            derive_retry_token_key(&secret),
            derive_new_token_key(&secret)
        );
    }

    #[test]
    fn new_token_roundtrip() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let secret = [0x24u8; 32];

        let token = encode_new_token(&addr, &secret);

        assert!(validate_new_token(&token, &addr, &secret).unwrap());
    }

    #[test]
    fn new_token_rejects_wrong_addr() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let wrong_addr: SocketAddr = "127.0.0.2:54321".parse().unwrap();
        let secret = [0x24u8; 32];

        let token = encode_new_token(&addr, &secret);

        assert!(!validate_new_token(&token, &wrong_addr, &secret).unwrap());
    }

    #[test]
    fn new_token_rejects_wrong_secret() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let secret = [0x24u8; 32];
        let wrong_secret = [0x25u8; 32];

        let token = encode_new_token(&addr, &secret);

        assert!(!validate_new_token(&token, &addr, &wrong_secret).unwrap());
    }

    #[test]
    fn new_token_ipv6_roundtrip() {
        let addr: SocketAddr = "[::1]:9999".parse().unwrap();
        let secret = [0x81u8; 32];

        let token = encode_new_token(&addr, &secret);

        assert!(validate_new_token(&token, &addr, &secret).unwrap());
    }

    #[test]
    fn new_token_rejects_retry_format() {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let secret = [0x42u8; 32];
        let token = encode_retry_token(&addr, &ConnectionId::from_slice(&[1, 2]), &secret);

        assert!(!validate_new_token(&token, &addr, &secret).unwrap());
    }
}
