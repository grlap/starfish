//! QUIC frame encoding and decoding (RFC 9000 §19).
//!
//! QUIC packets contain one or more frames. Each frame begins with a type
//! byte (varint-encoded) followed by type-specific fields.

pub mod ack;
pub mod crypto;
pub mod stream;

use crate::error::QuicError;
use crate::packet::{decode_varint, encode_varint, MAX_CID_LEN};

/// All QUIC frame types (RFC 9000 §19).
#[derive(Debug, Clone)]
pub enum Frame {
    /// PADDING (§19.1) — single zero byte, used to increase packet size.
    Padding,

    /// PING (§19.2) — elicits an ACK, keeps connection alive.
    Ping,

    /// ACK (§19.3) — acknowledges received packets.
    Ack(ack::AckFrame),

    /// RESET_STREAM (§19.4) — abruptly terminates sending on a stream.
    ResetStream {
        stream_id: u64,
        application_error_code: u64,
        final_size: u64,
    },

    /// STOP_SENDING (§19.5) — requests peer stop sending on a stream.
    StopSending {
        stream_id: u64,
        application_error_code: u64,
    },

    /// CRYPTO (§19.6) — carries TLS handshake data.
    Crypto(crypto::CryptoFrame),

    /// NEW_TOKEN (§19.7) — provides a token for future Initial packets.
    NewToken { token: Vec<u8> },

    /// STREAM (§19.8) — carries application data on a stream.
    Stream(stream::StreamFrame),

    /// MAX_DATA (§19.9) — connection-level flow control.
    MaxData { maximum_data: u64 },

    /// MAX_STREAM_DATA (§19.10) — stream-level flow control.
    MaxStreamData {
        stream_id: u64,
        maximum_stream_data: u64,
    },

    /// MAX_STREAMS (§19.11) — limits number of streams peer can open.
    MaxStreams {
        bidirectional: bool,
        maximum_streams: u64,
    },

    /// DATA_BLOCKED (§19.12) — connection-level flow control blocked.
    DataBlocked { maximum_data: u64 },

    /// STREAM_DATA_BLOCKED (§19.13) — stream-level flow control blocked.
    StreamDataBlocked {
        stream_id: u64,
        maximum_stream_data: u64,
    },

    /// STREAMS_BLOCKED (§19.14) — stream creation blocked by limit.
    StreamsBlocked {
        bidirectional: bool,
        maximum_streams: u64,
    },

    /// NEW_CONNECTION_ID (§19.15) — provides a new CID to the peer.
    NewConnectionId {
        sequence_number: u64,
        retire_prior_to: u64,
        connection_id: Vec<u8>,
        stateless_reset_token: [u8; 16],
    },

    /// RETIRE_CONNECTION_ID (§19.16) — retires a CID.
    RetireConnectionId { sequence_number: u64 },

    /// PATH_CHALLENGE (§19.17) — path validation.
    PathChallenge { data: [u8; 8] },

    /// PATH_RESPONSE (§19.18) — path validation response.
    PathResponse { data: [u8; 8] },

    /// CONNECTION_CLOSE (§19.19) — closes the connection.
    ConnectionClose {
        /// true = transport error (type 0x1c), false = application error (type 0x1d)
        is_transport: bool,
        error_code: u64,
        /// Only present for transport errors.
        frame_type: Option<u64>,
        reason: Vec<u8>,
    },

    /// HANDSHAKE_DONE (§19.20) — signals handshake completion (server only).
    HandshakeDone,
}

impl Frame {
    /// Returns true if this frame is ack-eliciting (RFC 9000 §13.2.1).
    pub fn is_ack_eliciting(&self) -> bool {
        !matches!(self, Frame::Ack(_) | Frame::Padding)
    }

    /// Returns true if this frame should be retransmitted when its packet is lost.
    /// ACK, PADDING, and PING frames are never retransmitted.
    pub fn is_retransmittable(&self) -> bool {
        !matches!(
            self,
            Frame::Ack(_) | Frame::Padding | Frame::Ping | Frame::PathChallenge { .. }
        )
    }

    /// Encode this frame into a buffer.
    pub fn encode(&self, buf: &mut Vec<u8>) -> Result<(), QuicError> {
        match self {
            Frame::Padding => buf.push(0x00),
            Frame::Ping => buf.push(0x01),
            Frame::Ack(ack) => ack.encode(buf)?,
            Frame::ResetStream {
                stream_id,
                application_error_code,
                final_size,
            } => {
                encode_varint(0x04, buf)?;
                encode_varint(*stream_id, buf)?;
                encode_varint(*application_error_code, buf)?;
                encode_varint(*final_size, buf)?;
            }
            Frame::StopSending {
                stream_id,
                application_error_code,
            } => {
                encode_varint(0x05, buf)?;
                encode_varint(*stream_id, buf)?;
                encode_varint(*application_error_code, buf)?;
            }
            Frame::Crypto(crypto) => crypto.encode(buf)?,
            Frame::NewToken { token } => {
                encode_varint(0x07, buf)?;
                encode_varint(token.len() as u64, buf)?;
                buf.extend_from_slice(token);
            }
            Frame::Stream(stream) => stream.encode(buf)?,
            Frame::MaxData { maximum_data } => {
                encode_varint(0x10, buf)?;
                encode_varint(*maximum_data, buf)?;
            }
            Frame::MaxStreamData {
                stream_id,
                maximum_stream_data,
            } => {
                encode_varint(0x11, buf)?;
                encode_varint(*stream_id, buf)?;
                encode_varint(*maximum_stream_data, buf)?;
            }
            Frame::MaxStreams {
                bidirectional,
                maximum_streams,
            } => {
                encode_varint(if *bidirectional { 0x12 } else { 0x13 }, buf)?;
                encode_varint(*maximum_streams, buf)?;
            }
            Frame::DataBlocked { maximum_data } => {
                encode_varint(0x14, buf)?;
                encode_varint(*maximum_data, buf)?;
            }
            Frame::StreamDataBlocked {
                stream_id,
                maximum_stream_data,
            } => {
                encode_varint(0x15, buf)?;
                encode_varint(*stream_id, buf)?;
                encode_varint(*maximum_stream_data, buf)?;
            }
            Frame::StreamsBlocked {
                bidirectional,
                maximum_streams,
            } => {
                encode_varint(if *bidirectional { 0x16 } else { 0x17 }, buf)?;
                encode_varint(*maximum_streams, buf)?;
            }
            Frame::NewConnectionId {
                sequence_number,
                retire_prior_to,
                connection_id,
                stateless_reset_token,
            } => {
                encode_varint(0x18, buf)?;
                encode_varint(*sequence_number, buf)?;
                encode_varint(*retire_prior_to, buf)?;
                buf.push(connection_id.len() as u8);
                buf.extend_from_slice(connection_id);
                buf.extend_from_slice(stateless_reset_token);
            }
            Frame::RetireConnectionId { sequence_number } => {
                encode_varint(0x19, buf)?;
                encode_varint(*sequence_number, buf)?;
            }
            Frame::PathChallenge { data } => {
                encode_varint(0x1a, buf)?;
                buf.extend_from_slice(data);
            }
            Frame::PathResponse { data } => {
                encode_varint(0x1b, buf)?;
                buf.extend_from_slice(data);
            }
            Frame::ConnectionClose {
                is_transport,
                error_code,
                frame_type,
                reason,
            } => {
                encode_varint(if *is_transport { 0x1c } else { 0x1d }, buf)?;
                encode_varint(*error_code, buf)?;
                if *is_transport {
                    encode_varint(frame_type.unwrap_or(0), buf)?;
                }
                encode_varint(reason.len() as u64, buf)?;
                buf.extend_from_slice(reason);
            }
            Frame::HandshakeDone => {
                encode_varint(0x1e, buf)?;
            }
        }

        Ok(())
    }
}

/// Decode a single frame from the buffer. Returns the frame and bytes consumed.
pub fn decode_frame(buf: &[u8]) -> Result<(Frame, usize), QuicError> {
    if buf.is_empty() {
        return Err(QuicError::InvalidFrame("empty frame data".into()));
    }

    // PADDING is a special case — just a zero byte
    if buf[0] == 0x00 {
        return Ok((Frame::Padding, 1));
    }

    let (frame_type, type_len) = decode_varint(buf)?;
    let mut offset = type_len;

    let frame = match frame_type {
        0x01 => Frame::Ping,

        // ACK (0x02, 0x03)
        0x02 | 0x03 => {
            let (ack, consumed) = ack::decode_ack_frame(&buf[offset..], frame_type == 0x03)?;
            offset += consumed;
            Frame::Ack(ack)
        }

        // RESET_STREAM
        0x04 => {
            let (stream_id, n) = decode_varint(&buf[offset..])?;
            offset += n;
            let (error_code, n) = decode_varint(&buf[offset..])?;
            offset += n;
            let (final_size, n) = decode_varint(&buf[offset..])?;
            offset += n;
            Frame::ResetStream {
                stream_id,
                application_error_code: error_code,
                final_size,
            }
        }

        // STOP_SENDING
        0x05 => {
            let (stream_id, n) = decode_varint(&buf[offset..])?;
            offset += n;
            let (error_code, n) = decode_varint(&buf[offset..])?;
            offset += n;
            Frame::StopSending {
                stream_id,
                application_error_code: error_code,
            }
        }

        // CRYPTO
        0x06 => {
            let (crypto, consumed) = crypto::decode_crypto_frame(&buf[offset..])?;
            offset += consumed;
            Frame::Crypto(crypto)
        }

        // NEW_TOKEN
        0x07 => {
            let (token_len, n) = decode_varint(&buf[offset..])?;
            offset += n;
            let token_len = token_len as usize;
            if buf.len() < offset + token_len {
                return Err(QuicError::InvalidFrame("NEW_TOKEN truncated".into()));
            }
            let token = buf[offset..offset + token_len].to_vec();
            offset += token_len;
            Frame::NewToken { token }
        }

        // STREAM (0x08..=0x0f)
        t @ 0x08..=0x0f => {
            let (stream_frame, consumed) = stream::decode_stream_frame(&buf[offset..], t as u8)?;
            offset += consumed;
            Frame::Stream(stream_frame)
        }

        // MAX_DATA
        0x10 => {
            let (max, n) = decode_varint(&buf[offset..])?;
            offset += n;
            Frame::MaxData { maximum_data: max }
        }

        // MAX_STREAM_DATA
        0x11 => {
            let (stream_id, n) = decode_varint(&buf[offset..])?;
            offset += n;
            let (max, n) = decode_varint(&buf[offset..])?;
            offset += n;
            Frame::MaxStreamData {
                stream_id,
                maximum_stream_data: max,
            }
        }

        // MAX_STREAMS (bidi=0x12, uni=0x13)
        0x12 | 0x13 => {
            let (max, n) = decode_varint(&buf[offset..])?;
            offset += n;
            Frame::MaxStreams {
                bidirectional: frame_type == 0x12,
                maximum_streams: max,
            }
        }

        // DATA_BLOCKED
        0x14 => {
            let (max, n) = decode_varint(&buf[offset..])?;
            offset += n;
            Frame::DataBlocked { maximum_data: max }
        }

        // STREAM_DATA_BLOCKED
        0x15 => {
            let (stream_id, n) = decode_varint(&buf[offset..])?;
            offset += n;
            let (max, n) = decode_varint(&buf[offset..])?;
            offset += n;
            Frame::StreamDataBlocked {
                stream_id,
                maximum_stream_data: max,
            }
        }

        // STREAMS_BLOCKED (bidi=0x16, uni=0x17)
        0x16 | 0x17 => {
            let (max, n) = decode_varint(&buf[offset..])?;
            offset += n;
            Frame::StreamsBlocked {
                bidirectional: frame_type == 0x16,
                maximum_streams: max,
            }
        }

        // NEW_CONNECTION_ID
        0x18 => {
            let (seq, n) = decode_varint(&buf[offset..])?;
            offset += n;
            let (retire, n) = decode_varint(&buf[offset..])?;
            offset += n;
            if offset >= buf.len() {
                return Err(QuicError::InvalidFrame(
                    "NEW_CONNECTION_ID truncated".into(),
                ));
            }
            let cid_len = buf[offset] as usize;
            if !(1..=MAX_CID_LEN).contains(&cid_len) {
                return Err(QuicError::InvalidFrame(
                    "NEW_CONNECTION_ID invalid connection ID length".into(),
                ));
            }

            offset += 1;
            if buf.len() < offset + cid_len + 16 {
                return Err(QuicError::InvalidFrame(
                    "NEW_CONNECTION_ID truncated".into(),
                ));
            }
            let connection_id = buf[offset..offset + cid_len].to_vec();
            offset += cid_len;
            let mut token = [0u8; 16];
            token.copy_from_slice(&buf[offset..offset + 16]);
            offset += 16;
            Frame::NewConnectionId {
                sequence_number: seq,
                retire_prior_to: retire,
                connection_id,
                stateless_reset_token: token,
            }
        }

        // RETIRE_CONNECTION_ID
        0x19 => {
            let (seq, n) = decode_varint(&buf[offset..])?;
            offset += n;
            Frame::RetireConnectionId {
                sequence_number: seq,
            }
        }

        // PATH_CHALLENGE
        0x1a => {
            if buf.len() < offset + 8 {
                return Err(QuicError::InvalidFrame("PATH_CHALLENGE truncated".into()));
            }
            let mut data = [0u8; 8];
            data.copy_from_slice(&buf[offset..offset + 8]);
            offset += 8;
            Frame::PathChallenge { data }
        }

        // PATH_RESPONSE
        0x1b => {
            if buf.len() < offset + 8 {
                return Err(QuicError::InvalidFrame("PATH_RESPONSE truncated".into()));
            }
            let mut data = [0u8; 8];
            data.copy_from_slice(&buf[offset..offset + 8]);
            offset += 8;
            Frame::PathResponse { data }
        }

        // CONNECTION_CLOSE (transport=0x1c, application=0x1d)
        0x1c | 0x1d => {
            let is_transport = frame_type == 0x1c;
            let (error_code, n) = decode_varint(&buf[offset..])?;
            offset += n;
            let frame_type_field = if is_transport {
                let (ft, n) = decode_varint(&buf[offset..])?;
                offset += n;
                Some(ft)
            } else {
                None
            };
            let (reason_len, n) = decode_varint(&buf[offset..])?;
            offset += n;
            let reason_len = reason_len as usize;
            if buf.len() < offset + reason_len {
                return Err(QuicError::InvalidFrame(
                    "CONNECTION_CLOSE reason truncated".into(),
                ));
            }
            let reason = buf[offset..offset + reason_len].to_vec();
            offset += reason_len;
            Frame::ConnectionClose {
                is_transport,
                error_code,
                frame_type: frame_type_field,
                reason,
            }
        }

        // HANDSHAKE_DONE
        0x1e => Frame::HandshakeDone,

        _ => {
            return Err(QuicError::InvalidFrame(format!(
                "unknown frame type: {frame_type:#x}"
            )));
        }
    };

    Ok((frame, offset))
}

/// Decode all frames from a decrypted packet payload.
pub fn decode_frames(mut buf: &[u8]) -> Result<Vec<Frame>, QuicError> {
    let mut frames = Vec::new();
    while !buf.is_empty() {
        let (frame, consumed) = decode_frame(buf)?;
        frames.push(frame);
        buf = &buf[consumed..];
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_roundtrip() {
        let frame = Frame::Padding;
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        assert_eq!(buf, vec![0x00]);
        let (decoded, consumed) = decode_frame(&buf).unwrap();
        assert_eq!(consumed, 1);
        assert!(matches!(decoded, Frame::Padding));
    }

    #[test]
    fn ping_roundtrip() {
        let frame = Frame::Ping;
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        assert!(matches!(decoded, Frame::Ping));
    }

    #[test]
    fn reset_stream_roundtrip() {
        let frame = Frame::ResetStream {
            stream_id: 4,
            application_error_code: 42,
            final_size: 1000,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::ResetStream {
                stream_id,
                application_error_code,
                final_size,
            } => {
                assert_eq!(stream_id, 4);
                assert_eq!(application_error_code, 42);
                assert_eq!(final_size, 1000);
            }
            other => panic!("expected ResetStream, got {:?}", other),
        }
    }

    #[test]
    fn max_data_roundtrip() {
        let frame = Frame::MaxData {
            maximum_data: 1_000_000,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::MaxData { maximum_data } => assert_eq!(maximum_data, 1_000_000),
            other => panic!("expected MaxData, got {:?}", other),
        }
    }

    #[test]
    fn connection_close_transport_roundtrip() {
        let frame = Frame::ConnectionClose {
            is_transport: true,
            error_code: 0x0a,
            frame_type: Some(0x08),
            reason: b"protocol violation".to_vec(),
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::ConnectionClose {
                is_transport,
                error_code,
                frame_type,
                reason,
            } => {
                assert!(is_transport);
                assert_eq!(error_code, 0x0a);
                assert_eq!(frame_type, Some(0x08));
                assert_eq!(reason, b"protocol violation");
            }
            other => panic!("expected ConnectionClose, got {:?}", other),
        }
    }

    #[test]
    fn connection_close_application_roundtrip() {
        let frame = Frame::ConnectionClose {
            is_transport: false,
            error_code: 42,
            frame_type: None,
            reason: b"app error".to_vec(),
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::ConnectionClose {
                is_transport,
                error_code,
                frame_type,
                reason,
            } => {
                assert!(!is_transport);
                assert_eq!(error_code, 42);
                assert_eq!(frame_type, None);
                assert_eq!(reason, b"app error");
            }
            other => panic!("expected ConnectionClose, got {:?}", other),
        }
    }

    #[test]
    fn handshake_done_roundtrip() {
        let frame = Frame::HandshakeDone;
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        assert!(matches!(decoded, Frame::HandshakeDone));
    }

    #[test]
    fn new_connection_id_roundtrip() {
        let frame = Frame::NewConnectionId {
            sequence_number: 1,
            retire_prior_to: 0,
            connection_id: vec![0x01, 0x02, 0x03, 0x04],
            stateless_reset_token: [0xaa; 16],
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::NewConnectionId {
                sequence_number,
                retire_prior_to,
                connection_id,
                stateless_reset_token,
            } => {
                assert_eq!(sequence_number, 1);
                assert_eq!(retire_prior_to, 0);
                assert_eq!(connection_id, vec![0x01, 0x02, 0x03, 0x04]);
                assert_eq!(stateless_reset_token, [0xaa; 16]);
            }
            other => panic!("expected NewConnectionId, got {:?}", other),
        }
    }

    #[test]
    fn new_connection_id_rejects_zero_length_cid() {
        let mut buf = Vec::new();
        encode_varint(0x18, &mut buf).unwrap();
        encode_varint(1, &mut buf).unwrap();
        encode_varint(0, &mut buf).unwrap();
        buf.push(0);
        buf.extend_from_slice(&[0xaa; 16]);

        assert!(decode_frame(&buf).is_err());
    }

    #[test]
    fn new_connection_id_rejects_oversized_cid() {
        let mut buf = Vec::new();
        encode_varint(0x18, &mut buf).unwrap();
        encode_varint(1, &mut buf).unwrap();
        encode_varint(0, &mut buf).unwrap();
        buf.push((MAX_CID_LEN + 1) as u8);
        buf.extend_from_slice(&[0xbb; MAX_CID_LEN + 1]);
        buf.extend_from_slice(&[0xaa; 16]);

        assert!(decode_frame(&buf).is_err());
    }

    #[test]
    fn path_challenge_roundtrip() {
        let data = [1, 2, 3, 4, 5, 6, 7, 8];
        let frame = Frame::PathChallenge { data };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::PathChallenge { data: d } => assert_eq!(d, data),
            other => panic!("expected PathChallenge, got {:?}", other),
        }
    }

    #[test]
    fn decode_multiple_frames() {
        let mut buf = Vec::new();
        Frame::Ping.encode(&mut buf).unwrap();
        Frame::MaxData {
            maximum_data: 65536,
        }
        .encode(&mut buf)
        .unwrap();
        Frame::HandshakeDone.encode(&mut buf).unwrap();

        let frames = decode_frames(&buf).unwrap();
        assert_eq!(frames.len(), 3);
        assert!(matches!(frames[0], Frame::Ping));
        assert!(matches!(
            frames[1],
            Frame::MaxData {
                maximum_data: 65536
            }
        ));
        assert!(matches!(frames[2], Frame::HandshakeDone));
    }

    #[test]
    fn stop_sending_roundtrip() {
        let frame = Frame::StopSending {
            stream_id: 8,
            application_error_code: 99,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::StopSending {
                stream_id,
                application_error_code,
            } => {
                assert_eq!(stream_id, 8);
                assert_eq!(application_error_code, 99);
            }
            other => panic!("expected StopSending, got {:?}", other),
        }
    }

    #[test]
    fn max_stream_data_roundtrip() {
        let frame = Frame::MaxStreamData {
            stream_id: 4,
            maximum_stream_data: 262_144,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::MaxStreamData {
                stream_id,
                maximum_stream_data,
            } => {
                assert_eq!(stream_id, 4);
                assert_eq!(maximum_stream_data, 262_144);
            }
            other => panic!("expected MaxStreamData, got {:?}", other),
        }
    }

    #[test]
    fn max_streams_bidi_roundtrip() {
        let frame = Frame::MaxStreams {
            bidirectional: true,
            maximum_streams: 100,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::MaxStreams {
                bidirectional,
                maximum_streams,
            } => {
                assert!(bidirectional);
                assert_eq!(maximum_streams, 100);
            }
            other => panic!("expected MaxStreams, got {:?}", other),
        }
    }

    #[test]
    fn max_streams_uni_roundtrip() {
        let frame = Frame::MaxStreams {
            bidirectional: false,
            maximum_streams: 50,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::MaxStreams {
                bidirectional,
                maximum_streams,
            } => {
                assert!(!bidirectional);
                assert_eq!(maximum_streams, 50);
            }
            other => panic!("expected MaxStreams, got {:?}", other),
        }
    }

    #[test]
    fn data_blocked_roundtrip() {
        let frame = Frame::DataBlocked {
            maximum_data: 65536,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::DataBlocked { maximum_data } => assert_eq!(maximum_data, 65536),
            other => panic!("expected DataBlocked, got {:?}", other),
        }
    }

    #[test]
    fn stream_data_blocked_roundtrip() {
        let frame = Frame::StreamDataBlocked {
            stream_id: 12,
            maximum_stream_data: 8192,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::StreamDataBlocked {
                stream_id,
                maximum_stream_data,
            } => {
                assert_eq!(stream_id, 12);
                assert_eq!(maximum_stream_data, 8192);
            }
            other => panic!("expected StreamDataBlocked, got {:?}", other),
        }
    }

    #[test]
    fn streams_blocked_bidi_roundtrip() {
        let frame = Frame::StreamsBlocked {
            bidirectional: true,
            maximum_streams: 10,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::StreamsBlocked {
                bidirectional,
                maximum_streams,
            } => {
                assert!(bidirectional);
                assert_eq!(maximum_streams, 10);
            }
            other => panic!("expected StreamsBlocked, got {:?}", other),
        }
    }

    #[test]
    fn streams_blocked_uni_roundtrip() {
        let frame = Frame::StreamsBlocked {
            bidirectional: false,
            maximum_streams: 5,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::StreamsBlocked {
                bidirectional,
                maximum_streams,
            } => {
                assert!(!bidirectional);
                assert_eq!(maximum_streams, 5);
            }
            other => panic!("expected StreamsBlocked, got {:?}", other),
        }
    }

    #[test]
    fn retire_connection_id_roundtrip() {
        let frame = Frame::RetireConnectionId {
            sequence_number: 42,
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::RetireConnectionId { sequence_number } => assert_eq!(sequence_number, 42),
            other => panic!("expected RetireConnectionId, got {:?}", other),
        }
    }

    #[test]
    fn path_response_roundtrip() {
        let data = [8, 7, 6, 5, 4, 3, 2, 1];
        let frame = Frame::PathResponse { data };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::PathResponse { data: d } => assert_eq!(d, data),
            other => panic!("expected PathResponse, got {:?}", other),
        }
    }

    #[test]
    fn new_token_roundtrip() {
        let frame = Frame::NewToken {
            token: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        let (decoded, _) = decode_frame(&buf).unwrap();
        match decoded {
            Frame::NewToken { token } => assert_eq!(token, vec![0xde, 0xad, 0xbe, 0xef]),
            other => panic!("expected NewToken, got {:?}", other),
        }
    }

    #[test]
    fn ack_eliciting() {
        assert!(!Frame::Padding.is_ack_eliciting());
        assert!(!Frame::Ack(ack::AckFrame {
            largest_acknowledged: 0,
            ack_delay: 0,
            ranges: vec![],
            ecn: None,
        })
        .is_ack_eliciting());
        assert!(Frame::Ping.is_ack_eliciting());
        assert!(Frame::HandshakeDone.is_ack_eliciting());
    }
}
