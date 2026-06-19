//! Typed error types for starfish-tls.
//!
//! [`TlsError`] replaces `Box<dyn Error>` in the public API, giving callers
//! the ability to match on specific failure modes during TLS connection setup.

use std::fmt;

/// Error returned by [`TlsClient::new`](crate::tls_client::TlsClient::new)
/// and [`TlsServer::new`](crate::tls_server::TlsServer::new).
#[derive(Debug)]
pub enum TlsError {
    /// The server name string is not a valid DNS name.
    InvalidServerName(rustls::pki_types::InvalidDnsNameError),

    /// rustls rejected the configuration or failed during handshake.
    Tls(rustls::Error),

    /// An I/O error occurred during the handshake.
    Io(std::io::Error),
}

// --- From impls: these are what make the `?` operator work ---

impl From<rustls::pki_types::InvalidDnsNameError> for TlsError {
    fn from(err: rustls::pki_types::InvalidDnsNameError) -> Self {
        TlsError::InvalidServerName(err)
    }
}

impl From<rustls::Error> for TlsError {
    fn from(err: rustls::Error) -> Self {
        TlsError::Tls(err)
    }
}

impl From<std::io::Error> for TlsError {
    fn from(err: std::io::Error) -> Self {
        TlsError::Io(err)
    }
}

// --- Display: human-readable error message ---

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsError::InvalidServerName(e) => write!(f, "invalid server name: {e}"),
            TlsError::Tls(e) => write!(f, "TLS error: {e}"),
            TlsError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

// --- Error: the `source()` method lets callers drill into the underlying cause ---

impl std::error::Error for TlsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TlsError::InvalidServerName(e) => Some(e),
            TlsError::Tls(e) => Some(e),
            TlsError::Io(e) => Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn from_invalid_dns_name() {
        use rustls::pki_types::ServerName;

        // "not a valid name!!!" is not a valid DNS name, so try_from fails.
        let dns_err = ServerName::try_from("not a valid name!!!".to_string()).unwrap_err();
        let tls_err = TlsError::from(dns_err);

        assert!(matches!(tls_err, TlsError::InvalidServerName(_)));
        assert!(tls_err.source().is_some());
        assert!(tls_err.to_string().contains("invalid server name"));
    }

    #[test]
    fn from_rustls_error() {
        let rustls_err = rustls::Error::General("test error".into());
        let tls_err = TlsError::from(rustls_err);

        assert!(matches!(tls_err, TlsError::Tls(_)));
        assert!(tls_err.source().is_some());
        assert!(tls_err.to_string().contains("TLS error"));
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let tls_err = TlsError::from(io_err);

        assert!(matches!(tls_err, TlsError::Io(_)));
        assert!(tls_err.source().is_some());
        assert!(tls_err.to_string().contains("I/O error"));
    }

    #[test]
    fn question_mark_converts_into_box_dyn_error() {
        // Verifies that TlsError works with `?` in functions returning Box<dyn Error>.
        fn returns_boxed() -> Result<(), Box<dyn Error>> {
            let err = TlsError::from(rustls::Error::General("test".into()));
            Err(err)?
        }
        let err = returns_boxed().unwrap_err();
        assert!(err.to_string().contains("TLS error"));
    }
}
