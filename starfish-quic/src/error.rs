//! Typed error types for starfish-quic.
//!
//! [`QuicError`] provides structured error variants for QUIC transport failures,
//! TLS errors, and protocol violations. Supports the `?` operator via `From` impls.

use std::fmt;

/// QUIC transport error codes (RFC 9000 §20.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum TransportErrorCode {
    NoError = 0x00,
    InternalError = 0x01,
    ConnectionRefused = 0x02,
    FlowControlError = 0x03,
    StreamLimitError = 0x04,
    StreamStateError = 0x05,
    FinalSizeError = 0x06,
    FrameEncodingError = 0x07,
    TransportParameterError = 0x08,
    ConnectionIdLimitError = 0x09,
    ProtocolViolation = 0x0a,
    InvalidToken = 0x0b,
    ApplicationError = 0x0c,
    CryptoBufferExceeded = 0x0d,
    KeyUpdateError = 0x0e,
    AeadLimitReached = 0x0f,
    NoViablePath = 0x10,
    /// TLS alert codes are 0x0100 + alert code.
    CryptoError(u8),
}

impl TransportErrorCode {
    pub fn from_u64(code: u64) -> Self {
        match code {
            0x00 => Self::NoError,
            0x01 => Self::InternalError,
            0x02 => Self::ConnectionRefused,
            0x03 => Self::FlowControlError,
            0x04 => Self::StreamLimitError,
            0x05 => Self::StreamStateError,
            0x06 => Self::FinalSizeError,
            0x07 => Self::FrameEncodingError,
            0x08 => Self::TransportParameterError,
            0x09 => Self::ConnectionIdLimitError,
            0x0a => Self::ProtocolViolation,
            0x0b => Self::InvalidToken,
            0x0c => Self::ApplicationError,
            0x0d => Self::CryptoBufferExceeded,
            0x0e => Self::KeyUpdateError,
            0x0f => Self::AeadLimitReached,
            0x10 => Self::NoViablePath,
            c if (0x0100..=0x01ff).contains(&c) => Self::CryptoError((c - 0x0100) as u8),
            _ => Self::InternalError,
        }
    }

    pub fn to_u64(self) -> u64 {
        match self {
            Self::NoError => 0x00,
            Self::InternalError => 0x01,
            Self::ConnectionRefused => 0x02,
            Self::FlowControlError => 0x03,
            Self::StreamLimitError => 0x04,
            Self::StreamStateError => 0x05,
            Self::FinalSizeError => 0x06,
            Self::FrameEncodingError => 0x07,
            Self::TransportParameterError => 0x08,
            Self::ConnectionIdLimitError => 0x09,
            Self::ProtocolViolation => 0x0a,
            Self::InvalidToken => 0x0b,
            Self::ApplicationError => 0x0c,
            Self::CryptoBufferExceeded => 0x0d,
            Self::KeyUpdateError => 0x0e,
            Self::AeadLimitReached => 0x0f,
            Self::NoViablePath => 0x10,
            Self::CryptoError(alert) => 0x0100 + alert as u64,
        }
    }
}

impl fmt::Display for TransportErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoError => write!(f, "NO_ERROR"),
            Self::InternalError => write!(f, "INTERNAL_ERROR"),
            Self::ConnectionRefused => write!(f, "CONNECTION_REFUSED"),
            Self::FlowControlError => write!(f, "FLOW_CONTROL_ERROR"),
            Self::StreamLimitError => write!(f, "STREAM_LIMIT_ERROR"),
            Self::StreamStateError => write!(f, "STREAM_STATE_ERROR"),
            Self::FinalSizeError => write!(f, "FINAL_SIZE_ERROR"),
            Self::FrameEncodingError => write!(f, "FRAME_ENCODING_ERROR"),
            Self::TransportParameterError => write!(f, "TRANSPORT_PARAMETER_ERROR"),
            Self::ConnectionIdLimitError => write!(f, "CONNECTION_ID_LIMIT_ERROR"),
            Self::ProtocolViolation => write!(f, "PROTOCOL_VIOLATION"),
            Self::InvalidToken => write!(f, "INVALID_TOKEN"),
            Self::ApplicationError => write!(f, "APPLICATION_ERROR"),
            Self::CryptoBufferExceeded => write!(f, "CRYPTO_BUFFER_EXCEEDED"),
            Self::KeyUpdateError => write!(f, "KEY_UPDATE_ERROR"),
            Self::AeadLimitReached => write!(f, "AEAD_LIMIT_REACHED"),
            Self::NoViablePath => write!(f, "NO_VIABLE_PATH"),
            Self::CryptoError(alert) => write!(f, "CRYPTO_ERROR(0x{:02x})", alert),
        }
    }
}

/// Error returned by QUIC operations.
#[derive(Debug)]
pub enum QuicError {
    /// An I/O error occurred on the underlying UDP socket.
    Io(std::io::Error),

    /// A TLS error occurred during the QUIC-TLS handshake.
    Tls(rustls::Error),

    /// A QUIC transport error (RFC 9000 §20).
    Transport(TransportErrorCode, String),

    /// Received a malformed or unrecognizable packet.
    InvalidPacket(String),

    /// A stream was reset by the peer with an application error code.
    StreamReset(u64),

    /// The connection was closed by the peer.
    ConnectionClosed {
        error_code: TransportErrorCode,
        reason: String,
    },

    /// The connection has entered the draining or closed state.
    ConnectionDraining,

    /// A frame encoding or decoding error.
    InvalidFrame(String),

    /// Buffer too small for the requested operation.
    BufferTooSmall,
}

// --- From impls for ? operator ---

impl From<std::io::Error> for QuicError {
    fn from(err: std::io::Error) -> Self {
        QuicError::Io(err)
    }
}

impl From<rustls::Error> for QuicError {
    fn from(err: rustls::Error) -> Self {
        QuicError::Tls(err)
    }
}

// --- Display ---

impl fmt::Display for QuicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuicError::Io(e) => write!(f, "I/O error: {e}"),
            QuicError::Tls(e) => write!(f, "TLS error: {e}"),
            QuicError::Transport(code, reason) => {
                write!(f, "transport error {code}: {reason}")
            }
            QuicError::InvalidPacket(msg) => write!(f, "invalid packet: {msg}"),
            QuicError::StreamReset(code) => write!(f, "stream reset with code {code}"),
            QuicError::ConnectionClosed { error_code, reason } => {
                write!(f, "connection closed: {error_code} ({reason})")
            }
            QuicError::ConnectionDraining => write!(f, "connection draining"),
            QuicError::InvalidFrame(msg) => write!(f, "invalid frame: {msg}"),
            QuicError::BufferTooSmall => write!(f, "buffer too small"),
        }
    }
}

// --- Error trait ---

impl std::error::Error for QuicError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            QuicError::Io(e) => Some(e),
            QuicError::Tls(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let quic_err = QuicError::from(io_err);
        assert!(matches!(quic_err, QuicError::Io(_)));
        assert!(quic_err.source().is_some());
        assert!(quic_err.to_string().contains("I/O error"));
    }

    #[test]
    fn transport_error_code_roundtrip() {
        for code in [0x00, 0x01, 0x03, 0x0a, 0x10] {
            let tec = TransportErrorCode::from_u64(code);
            assert_eq!(tec.to_u64(), code);
        }
    }

    #[test]
    fn crypto_error_code_roundtrip() {
        let tec = TransportErrorCode::from_u64(0x0148); // TLS alert 72
        assert!(matches!(tec, TransportErrorCode::CryptoError(72)));
        assert_eq!(tec.to_u64(), 0x0148);
    }

    #[test]
    fn question_mark_converts_into_box_dyn_error() {
        fn returns_boxed() -> Result<(), Box<dyn Error>> {
            let err = QuicError::InvalidPacket("test".into());
            Err(err)?
        }
        let err = returns_boxed().unwrap_err();
        assert!(err.to_string().contains("invalid packet"));
    }
}
