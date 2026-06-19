//! AEAD encryption and decryption for QUIC packets (RFC 9001 §5.3).
//!
//! QUIC uses AEAD (Authenticated Encryption with Associated Data) to protect
//! packet payloads. The header (up to and including the packet number) serves
//! as the AAD.

use crate::error::QuicError;
use rustls::quic::PacketKey;

/// Encrypt a QUIC packet payload in place.
///
/// - `pn`: the full packet number (used to construct the nonce)
/// - `header`: the packet header bytes (used as AAD), must include the packet number
/// - `payload`: the plaintext payload to encrypt (modified in place, tag appended)
/// - `key`: the AEAD key for this packet number space
pub fn encrypt_packet(
    pn: u64,
    header: &[u8],
    payload: &mut Vec<u8>,
    key: &dyn PacketKey,
) -> Result<(), QuicError> {
    let tag_len = key.tag_len();
    // Reserve space for the AEAD tag
    let payload_len = payload.len();
    payload.resize(payload_len + tag_len, 0);

    let tag = key
        .encrypt_in_place(pn, header, &mut payload[..payload_len])
        .map_err(|_| QuicError::InvalidPacket("AEAD encryption failed".into()))?;

    // Append the authentication tag
    payload[payload_len..].copy_from_slice(tag.as_ref());

    Ok(())
}

/// Decrypt a QUIC packet payload in place.
///
/// - `pn`: the reconstructed full packet number
/// - `header`: the packet header bytes (used as AAD)
/// - `payload`: the ciphertext payload (modified in place, tag removed)
/// - `key`: the AEAD key for this packet number space
///
/// Returns the plaintext length (payload is truncated to this length).
pub fn decrypt_packet(
    pn: u64,
    header: &[u8],
    payload: &mut [u8],
    key: &dyn PacketKey,
) -> Result<usize, QuicError> {
    let plaintext = key
        .decrypt_in_place(pn, header, payload)
        .map_err(|_| QuicError::InvalidPacket("AEAD decryption failed".into()))?;

    Ok(plaintext.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keys::derive_initial_keys;
    use crate::packet::ConnectionId;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let dcid = ConnectionId::from_slice(&[0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08]);
        let provider = rustls::crypto::ring::default_provider();
        let keys = derive_initial_keys(dcid.as_slice(), true, &provider).unwrap();

        let pn = 0u64;
        let header = b"fake_header_aad";
        let plaintext = b"Hello, QUIC!".to_vec();

        // Encrypt with local key
        let mut ciphertext = plaintext.clone();
        encrypt_packet(pn, header, &mut ciphertext, keys.local.packet.as_ref()).unwrap();
        assert_ne!(ciphertext[..plaintext.len()], plaintext[..]);

        // Decrypt with remote key (other side)
        let keys_server = derive_initial_keys(dcid.as_slice(), false, &provider).unwrap();
        let pt_len = decrypt_packet(
            pn,
            header,
            &mut ciphertext,
            keys_server.remote.packet.as_ref(),
        )
        .unwrap();
        assert_eq!(&ciphertext[..pt_len], &plaintext[..]);
    }
}
