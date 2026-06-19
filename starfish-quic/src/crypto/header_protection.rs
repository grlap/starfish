//! QUIC header protection (RFC 9001 §5.4).
//!
//! After AEAD encryption, the packet number and parts of the first byte
//! are masked using a sample from the ciphertext. This prevents middleboxes
//! from reading packet numbers.

use crate::error::QuicError;
use rustls::quic::HeaderProtectionKey;

/// Apply header protection to an outgoing packet.
///
/// `buf` is the full packet, `pn_offset` is where the packet number starts,
/// `pn_len` is how many bytes the packet number occupies (1–4).
pub fn apply_header_protection(
    buf: &mut [u8],
    pn_offset: usize,
    pn_len: usize,
    key: &dyn HeaderProtectionKey,
) -> Result<(), QuicError> {
    let sample_len = key.sample_len();
    let sample_offset = pn_offset + 4;
    if buf.len() < sample_offset + sample_len {
        return Err(QuicError::InvalidPacket(
            "packet too short for header protection sample".into(),
        ));
    }

    let sample = buf[sample_offset..sample_offset + sample_len].to_vec();

    if pn_offset == 0 || buf.is_empty() {
        return Err(QuicError::InvalidPacket(
            "pn_offset must be > 0 and buffer non-empty for header protection".into(),
        ));
    }

    // Split buf to get mutable references to first byte and packet number
    let (first, rest) = buf
        .split_first_mut()
        .ok_or_else(|| QuicError::InvalidPacket("empty buffer".into()))?;
    let pn_bytes = &mut rest[pn_offset - 1..pn_offset - 1 + pn_len];

    key.encrypt_in_place(&sample, first, pn_bytes)
        .map_err(|_| QuicError::InvalidPacket("header protection encrypt failed".into()))?;

    Ok(())
}

/// Remove header protection from an incoming packet.
///
/// `buf` is the full packet. `pn_offset` is where the packet number starts.
/// Returns the packet number length (1–4) decoded from the unmasked first byte.
pub fn remove_header_protection(
    buf: &mut [u8],
    pn_offset: usize,
    key: &dyn HeaderProtectionKey,
) -> Result<usize, QuicError> {
    let sample_len = key.sample_len();
    let sample_offset = pn_offset + 4;
    if buf.len() < sample_offset + sample_len {
        return Err(QuicError::InvalidPacket(
            "packet too short for header protection sample".into(),
        ));
    }

    let sample = buf[sample_offset..sample_offset + sample_len].to_vec();
    let max_pn = 4.min(buf.len() - pn_offset);

    if pn_offset == 0 || buf.is_empty() {
        return Err(QuicError::InvalidPacket(
            "pn_offset must be > 0 and buffer non-empty for header protection".into(),
        ));
    }

    // Split buf to get mutable references to first byte and packet number
    let (first, rest) = buf
        .split_first_mut()
        .ok_or_else(|| QuicError::InvalidPacket("empty buffer".into()))?;
    let pn_bytes = &mut rest[pn_offset - 1..pn_offset - 1 + max_pn];

    key.decrypt_in_place(&sample, first, pn_bytes)
        .map_err(|_| QuicError::InvalidPacket("header protection decrypt failed".into()))?;

    // Read the packet number length from the now-unmasked first byte
    let pn_len = ((*first & 0x03) + 1) as usize;

    Ok(pn_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keys::derive_initial_keys;
    use crate::packet::ConnectionId;

    #[test]
    fn apply_remove_roundtrip() {
        let dcid = ConnectionId::from_slice(&[0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08]);
        let provider = rustls::crypto::ring::default_provider();
        let keys = derive_initial_keys(dcid.as_slice(), true, &provider).unwrap();

        // Build a fake packet:
        // header (with pn_len=1 in lower 2 bits = 0b00) + PN byte + 20+ bytes "ciphertext"
        let mut buf = vec![0xc0u8]; // long header, pn_len bits = 0 (meaning 1 byte)
        let pn_offset = 1;
        buf.push(0x00); // packet number = 0
        buf.extend(vec![0xaa; 24]); // fake ciphertext for sampling

        let original_first_byte = buf[0];
        let original_pn = buf[pn_offset];

        // Apply header protection
        apply_header_protection(&mut buf, pn_offset, 1, keys.local.header.as_ref()).unwrap();

        // Verify something changed
        assert!(buf[0] != original_first_byte || buf[pn_offset] != original_pn);

        // Remove header protection
        let pn_len =
            remove_header_protection(&mut buf, pn_offset, keys.local.header.as_ref()).unwrap();
        assert_eq!(pn_len, 1);
        assert_eq!(buf[0], original_first_byte);
        assert_eq!(buf[pn_offset], original_pn);
    }
}
