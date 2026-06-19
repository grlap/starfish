//! QUIC packet protection key management (RFC 9001 §5).
//!
//! Initial keys are derived from the client's first Destination Connection ID
//! using HKDF. Handshake and 1-RTT keys come from rustls via the
//! `quic::Keys` API.

use crate::packet::header::PacketNumberSpace;
use rustls::quic;

/// Holds the packet protection keys for all three packet number spaces.
pub struct PacketKeys {
    /// Initial keys (derived from DCID).
    pub initial: Option<DirectionalKeys>,
    /// Handshake keys (from TLS).
    pub handshake: Option<DirectionalKeys>,
    /// 0-RTT client send keys.
    pub zero_rtt_local: Option<quic::DirectionalKeys>,
    /// 0-RTT server receive keys.
    pub zero_rtt_remote: Option<quic::DirectionalKeys>,
    /// 1-RTT / Application Data keys (from TLS).
    pub one_rtt: Option<DirectionalKeys>,
}

/// A pair of keys for one direction (local seal + remote open).
pub struct DirectionalKeys {
    /// Keys for encrypting packets we send.
    pub local: Box<dyn quic::PacketKey>,
    /// Keys for decrypting packets we receive.
    pub remote: Box<dyn quic::PacketKey>,
    /// Header protection for packets we send.
    pub local_header: Box<dyn quic::HeaderProtectionKey>,
    /// Header protection for packets we receive.
    pub remote_header: Box<dyn quic::HeaderProtectionKey>,
}

impl Default for PacketKeys {
    fn default() -> Self {
        Self::new()
    }
}

impl PacketKeys {
    pub fn new() -> Self {
        Self {
            initial: None,
            handshake: None,
            zero_rtt_local: None,
            zero_rtt_remote: None,
            one_rtt: None,
        }
    }

    /// Get the keys for a given packet number space.
    pub fn get(&self, space: PacketNumberSpace) -> Option<&DirectionalKeys> {
        match space {
            PacketNumberSpace::Initial => self.initial.as_ref(),
            PacketNumberSpace::Handshake => self.handshake.as_ref(),
            PacketNumberSpace::ApplicationData => self.one_rtt.as_ref(),
        }
    }

    /// Set keys for a given packet number space from rustls quic::Keys.
    pub fn set_from_quic_keys(&mut self, space: PacketNumberSpace, keys: quic::Keys) {
        let dir = DirectionalKeys {
            local: keys.local.packet,
            remote: keys.remote.packet,
            local_header: keys.local.header,
            remote_header: keys.remote.header,
        };
        match space {
            PacketNumberSpace::Initial => self.initial = Some(dir),
            PacketNumberSpace::Handshake => self.handshake = Some(dir),
            PacketNumberSpace::ApplicationData => self.one_rtt = Some(dir),
        }
    }

    /// Set 0-RTT keys from rustls directional keys.
    pub fn set_zero_rtt(&mut self, is_client: bool, keys: quic::DirectionalKeys) {
        if is_client {
            self.zero_rtt_local = Some(keys);
            self.zero_rtt_remote = None;
        } else {
            self.zero_rtt_remote = Some(keys);
            self.zero_rtt_local = None;
        }
    }

    /// Get the local 0-RTT keys, if installed.
    pub fn zero_rtt_local(&self) -> Option<&quic::DirectionalKeys> {
        self.zero_rtt_local.as_ref()
    }

    /// Get the remote 0-RTT keys, if installed.
    pub fn zero_rtt_remote(&self) -> Option<&quic::DirectionalKeys> {
        self.zero_rtt_remote.as_ref()
    }

    /// Discard 0-RTT keys.
    pub fn discard_zero_rtt(&mut self) {
        self.zero_rtt_local = None;
        self.zero_rtt_remote = None;
    }

    /// Discard keys for a packet number space (RFC 9001 §4.9).
    pub fn discard(&mut self, space: PacketNumberSpace) {
        match space {
            PacketNumberSpace::Initial => self.initial = None,
            PacketNumberSpace::Handshake => self.handshake = None,
            PacketNumberSpace::ApplicationData => self.one_rtt = None,
        }
    }
}

/// Derive Initial keys from the client's first Destination Connection ID.
///
/// Both client and server derive the same pair of keys — the `is_client`
/// flag determines which direction maps to local vs remote.
pub fn derive_initial_keys(
    dcid: &[u8],
    is_client: bool,
    crypto_provider: &rustls::crypto::CryptoProvider,
) -> Result<quic::Keys, crate::error::QuicError> {
    const INITIAL_CIPHER_SUITE: rustls::CipherSuite = rustls::CipherSuite::TLS13_AES_128_GCM_SHA256;
    let suite = crypto_provider
        .cipher_suites
        .iter()
        .find_map(|cs| match (cs.suite(), cs.tls13()) {
            (INITIAL_CIPHER_SUITE, Some(tls13)) => tls13.quic_suite(),
            _ => None,
        })
        .ok_or_else(|| {
            crate::error::QuicError::Tls(rustls::Error::General(
                "TLS_AES_128_GCM_SHA256 QUIC suite is unavailable".into(),
            ))
        })?;

    let keys = if is_client {
        suite.keys(dcid, rustls::Side::Client, quic::Version::V1)
    } else {
        suite.keys(dcid, rustls::Side::Server, quic::Version::V1)
    };

    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::ConnectionId;
    use rustls::CipherSuite;

    #[test]
    fn derive_initial_keys_succeeds() {
        let dcid = ConnectionId::from_slice(&[0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08]);
        let provider = rustls::crypto::ring::default_provider();

        let keys = derive_initial_keys(dcid.as_slice(), true, &provider).unwrap();
        // Keys should be non-null — verify we can get tag lengths
        assert!(keys.local.packet.tag_len() > 0);
        assert!(keys.remote.packet.tag_len() > 0);
    }

    #[test]
    fn packet_keys_lifecycle() {
        let mut pk = PacketKeys::new();
        assert!(pk.get(PacketNumberSpace::Initial).is_none());
        assert!(pk.get(PacketNumberSpace::Handshake).is_none());
        assert!(pk.get(PacketNumberSpace::ApplicationData).is_none());
        assert!(pk.zero_rtt_local().is_none());
        assert!(pk.zero_rtt_remote().is_none());

        let dcid = ConnectionId::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let provider = rustls::crypto::ring::default_provider();
        let keys = derive_initial_keys(dcid.as_slice(), true, &provider).unwrap();

        pk.set_from_quic_keys(PacketNumberSpace::Initial, keys);
        assert!(pk.get(PacketNumberSpace::Initial).is_some());

        pk.discard(PacketNumberSpace::Initial);
        assert!(pk.get(PacketNumberSpace::Initial).is_none());
    }

    #[test]
    fn derive_initial_keys_uses_tls_aes_128_gcm_sha256() {
        let dcid = ConnectionId::from_slice(&[0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08]);
        let provider = rustls::crypto::ring::default_provider();
        let derived = derive_initial_keys(dcid.as_slice(), true, &provider).unwrap();
        let expected_suite = provider
            .cipher_suites
            .iter()
            .find_map(|cs| match (cs.suite(), cs.tls13()) {
                (CipherSuite::TLS13_AES_128_GCM_SHA256, Some(tls13)) => tls13.quic_suite(),
                _ => None,
            })
            .unwrap();
        let expected =
            expected_suite.keys(dcid.as_slice(), rustls::Side::Client, quic::Version::V1);

        let header = b"test-header";
        let mut derived_payload = b"test-payload".to_vec();
        let derived_tag = derived
            .local
            .packet
            .encrypt_in_place(1, header, &mut derived_payload)
            .unwrap();

        let mut expected_payload = b"test-payload".to_vec();
        let expected_tag = expected
            .local
            .packet
            .encrypt_in_place(1, header, &mut expected_payload)
            .unwrap();

        assert_eq!(derived_payload, expected_payload);
        assert_eq!(derived_tag.as_ref(), expected_tag.as_ref());
    }
}
