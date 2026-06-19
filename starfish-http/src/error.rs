//! HTTP error types.
//!
//! [`HttpError`] covers failures from both HTTP/1.1 and HTTP/3 paths,
//! including I/O, TLS, QUIC transport, parsing, and QPACK errors.

use std::fmt;

/// Unified error type for the HTTP crate.
#[derive(Debug)]
pub enum HttpError {
    /// Underlying I/O error.
    Io(std::io::Error),
    /// TLS error from `starfish-tls`.
    Tls(starfish_tls::error::TlsError),
    /// QUIC transport error.
    Quic(starfish_quic::QuicError),
    /// Malformed request or response (parse failure).
    Parse(String),
    /// Header section exceeds maximum allowed size.
    HeaderTooLarge,
    /// Invalid chunked transfer-encoding.
    InvalidChunkEncoding,
    /// Connection closed before expected data was received.
    UnexpectedEof,
    /// Peer closed the connection.
    ConnectionClosed,
    /// Operation timed out.
    Timeout,
    /// QPACK header compression error.
    Qpack(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::Tls(e) => write!(f, "TLS error: {e}"),
            Self::Quic(e) => write!(f, "QUIC error: {e}"),
            Self::Parse(msg) => write!(f, "parse error: {msg}"),
            Self::HeaderTooLarge => write!(f, "header section too large"),
            Self::InvalidChunkEncoding => write!(f, "invalid chunked transfer-encoding"),
            Self::UnexpectedEof => write!(f, "unexpected end of stream"),
            Self::ConnectionClosed => write!(f, "connection closed"),
            Self::Timeout => write!(f, "timeout"),
            Self::Qpack(msg) => write!(f, "QPACK error: {msg}"),
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Tls(e) => Some(e),
            Self::Quic(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<starfish_tls::error::TlsError> for HttpError {
    fn from(e: starfish_tls::error::TlsError) -> Self {
        Self::Tls(e)
    }
}

impl From<starfish_quic::QuicError> for HttpError {
    fn from(e: starfish_quic::QuicError) -> Self {
        Self::Quic(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn wrapped_errors_preserve_display_and_source() {
        let io_error = HttpError::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        assert!(io_error.to_string().contains("IO error"));
        assert!(io_error.source().is_some());

        let tls_error = HttpError::from(starfish_tls::error::TlsError::from(
            rustls::Error::General("bad tls".into()),
        ));
        assert!(tls_error.to_string().contains("TLS error"));
        assert!(tls_error.source().is_some());

        let quic_error =
            HttpError::from(starfish_quic::QuicError::InvalidPacket("bad packet".into()));
        assert!(quic_error.to_string().contains("QUIC error"));
        assert!(quic_error.source().is_some());
    }

    #[test]
    fn leaf_variants_render_expected_messages() {
        assert_eq!(
            HttpError::HeaderTooLarge.to_string(),
            "header section too large"
        );
        assert_eq!(
            HttpError::InvalidChunkEncoding.to_string(),
            "invalid chunked transfer-encoding"
        );
        assert_eq!(
            HttpError::UnexpectedEof.to_string(),
            "unexpected end of stream"
        );
        assert_eq!(HttpError::ConnectionClosed.to_string(), "connection closed");
        assert_eq!(HttpError::Timeout.to_string(), "timeout");
        assert_eq!(
            HttpError::Qpack("decoder failed".into()).to_string(),
            "QPACK error: decoder failed"
        );
    }
}
