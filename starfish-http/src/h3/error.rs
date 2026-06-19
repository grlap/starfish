//! HTTP/3 application error codes (RFC 9114 §8.1).
//!
//! These error codes are used in QUIC `CONNECTION_CLOSE` (type 0x1d) and
//! `RESET_STREAM` frames to signal HTTP/3-level protocol errors to the peer.

/// No error. Used when the connection or stream needs to be closed but there
/// is no error to signal.
pub const H3_NO_ERROR: u64 = 0x0100;

/// Peer violated protocol requirements in a way that does not match a more
/// specific error code, or endpoint refuses to use a more specific error code.
pub const H3_GENERAL_PROTOCOL_ERROR: u64 = 0x0101;

/// An internal error has occurred in the HTTP stack.
pub const H3_INTERNAL_ERROR: u64 = 0x0102;

/// The endpoint detected that its peer created a stream that it will not accept.
pub const H3_STREAM_CREATION_ERROR: u64 = 0x0103;

/// A stream required by the HTTP/3 connection was closed or reset.
pub const H3_CLOSED_CRITICAL_STREAM: u64 = 0x0104;

/// A frame was received that is not permitted in the current state or on the
/// current stream.
pub const H3_FRAME_UNEXPECTED: u64 = 0x0105;

/// A frame that fails to satisfy layout requirements or exceeds the size that
/// was advertised was received.
pub const H3_FRAME_ERROR: u64 = 0x0106;

/// The endpoint detected that its peer is exhibiting behavior that might be
/// generating excessive load.
pub const H3_EXCESSIVE_LOAD: u64 = 0x0107;

/// A Stream ID or Push ID was used incorrectly, such as exceeding a limit,
/// reducing a limit, or being reused.
pub const H3_ID_ERROR: u64 = 0x0108;

/// An endpoint detected an error in the payload of a SETTINGS frame.
pub const H3_SETTINGS_ERROR: u64 = 0x0109;

/// No SETTINGS frame was received at the beginning of the control stream.
pub const H3_MISSING_SETTINGS: u64 = 0x010a;

/// A server rejected a request without performing any application processing.
pub const H3_REQUEST_REJECTED: u64 = 0x010b;

/// The request or its response (including pushed response) is cancelled.
pub const H3_REQUEST_CANCELLED: u64 = 0x010c;

/// The client's stream terminated without containing a fully formed request.
pub const H3_REQUEST_INCOMPLETE: u64 = 0x010d;

/// An HTTP message was malformed and cannot be processed.
pub const H3_MESSAGE_ERROR: u64 = 0x010e;

/// The TCP connection established in response to a CONNECT request was reset
/// or abnormally closed.
pub const H3_CONNECT_ERROR: u64 = 0x010f;

/// The requested operation cannot be served over HTTP/3. The peer should
/// retry over HTTP/1.1.
pub const H3_VERSION_FALLBACK: u64 = 0x0110;

/// Returns a human-readable name for an HTTP/3 error code.
pub fn error_code_name(code: u64) -> &'static str {
    match code {
        H3_NO_ERROR => "H3_NO_ERROR",
        H3_GENERAL_PROTOCOL_ERROR => "H3_GENERAL_PROTOCOL_ERROR",
        H3_INTERNAL_ERROR => "H3_INTERNAL_ERROR",
        H3_STREAM_CREATION_ERROR => "H3_STREAM_CREATION_ERROR",
        H3_CLOSED_CRITICAL_STREAM => "H3_CLOSED_CRITICAL_STREAM",
        H3_FRAME_UNEXPECTED => "H3_FRAME_UNEXPECTED",
        H3_FRAME_ERROR => "H3_FRAME_ERROR",
        H3_EXCESSIVE_LOAD => "H3_EXCESSIVE_LOAD",
        H3_ID_ERROR => "H3_ID_ERROR",
        H3_SETTINGS_ERROR => "H3_SETTINGS_ERROR",
        H3_MISSING_SETTINGS => "H3_MISSING_SETTINGS",
        H3_REQUEST_REJECTED => "H3_REQUEST_REJECTED",
        H3_REQUEST_CANCELLED => "H3_REQUEST_CANCELLED",
        H3_REQUEST_INCOMPLETE => "H3_REQUEST_INCOMPLETE",
        H3_MESSAGE_ERROR => "H3_MESSAGE_ERROR",
        H3_CONNECT_ERROR => "H3_CONNECT_ERROR",
        H3_VERSION_FALLBACK => "H3_VERSION_FALLBACK",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_values_match_rfc() {
        assert_eq!(H3_NO_ERROR, 0x0100);
        assert_eq!(H3_GENERAL_PROTOCOL_ERROR, 0x0101);
        assert_eq!(H3_INTERNAL_ERROR, 0x0102);
        assert_eq!(H3_STREAM_CREATION_ERROR, 0x0103);
        assert_eq!(H3_CLOSED_CRITICAL_STREAM, 0x0104);
        assert_eq!(H3_FRAME_UNEXPECTED, 0x0105);
        assert_eq!(H3_FRAME_ERROR, 0x0106);
        assert_eq!(H3_EXCESSIVE_LOAD, 0x0107);
        assert_eq!(H3_ID_ERROR, 0x0108);
        assert_eq!(H3_SETTINGS_ERROR, 0x0109);
        assert_eq!(H3_MISSING_SETTINGS, 0x010a);
        assert_eq!(H3_REQUEST_REJECTED, 0x010b);
        assert_eq!(H3_REQUEST_CANCELLED, 0x010c);
        assert_eq!(H3_REQUEST_INCOMPLETE, 0x010d);
        assert_eq!(H3_MESSAGE_ERROR, 0x010e);
        assert_eq!(H3_CONNECT_ERROR, 0x010f);
        assert_eq!(H3_VERSION_FALLBACK, 0x0110);
    }

    #[test]
    fn error_code_name_known() {
        assert_eq!(error_code_name(H3_NO_ERROR), "H3_NO_ERROR");
        assert_eq!(error_code_name(H3_SETTINGS_ERROR), "H3_SETTINGS_ERROR");
    }

    #[test]
    fn error_code_name_unknown() {
        assert_eq!(error_code_name(0xffff), "UNKNOWN");
    }
}
