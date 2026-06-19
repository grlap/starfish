//! HTTP/3 frame encoding and decoding (RFC 9114 §7).
//!
//! HTTP/3 frames are carried inside QUIC streams and use variable-length
//! integer encoding for frame type and length.

use crate::error::HttpError;
use starfish_quic::{decode_varint as quic_decode_varint, encode_varint};

/// HTTP/3 frame types.
#[derive(Debug, Clone, PartialEq)]
pub enum H3Frame {
    /// DATA frame (type 0x00) — carries request/response body.
    Data(Vec<u8>),
    /// HEADERS frame (type 0x01) — QPACK-encoded header block.
    Headers(Vec<u8>),
    /// CANCEL_PUSH frame (type 0x03).
    CancelPush(u64),
    /// SETTINGS frame (type 0x04).
    Settings(Vec<H3Setting>),
    /// PUSH_PROMISE frame (type 0x05).
    PushPromise { push_id: u64, headers: Vec<u8> },
    /// GOAWAY frame (type 0x07).
    GoAway(u64),
    /// MAX_PUSH_ID frame (type 0x0d).
    MaxPushId(u64),
    /// PRIORITY_UPDATE request frame (RFC 9218 §7.1, type 0xF0700).
    PriorityUpdateRequest { stream_id: u64, value: Vec<u8> },
    /// PRIORITY_UPDATE push frame (RFC 9218 §7.2, type 0xF0701).
    PriorityUpdatePush { push_id: u64, value: Vec<u8> },
    /// Unknown/reserved frame type — skip.
    Unknown { frame_type: u64, data: Vec<u8> },
}

/// A single HTTP/3 setting key-value pair.
#[derive(Debug, Clone, PartialEq)]
pub struct H3Setting {
    pub id: u64,
    pub value: u64,
}

/// Well-known HTTP/3 setting identifiers.
pub mod setting_id {
    /// QPACK max table capacity.
    pub const QPACK_MAX_TABLE_CAPACITY: u64 = 0x01;
    /// Max header list size.
    pub const MAX_FIELD_SECTION_SIZE: u64 = 0x06;
    /// QPACK blocked streams.
    pub const QPACK_BLOCKED_STREAMS: u64 = 0x07;
}

// Frame/setting identifiers reserved by RFC 9114 because they were used by HTTP/2.
// Receipt of any of these is a connection error (H3_FRAME_UNEXPECTED for frame
// types per §7.2.8; H3_SETTINGS_ERROR for setting IDs per §7.2.4.1 / §11.6) — they
// must be rejected, NOT skipped like generic unknown/extension frames.
pub(crate) fn is_reserved_http2_setting_id(setting_id: u64) -> bool {
    matches!(setting_id, 0x00 | 0x02 | 0x03 | 0x04 | 0x05)
}

fn is_reserved_http2_frame_type(frame_type: u64) -> bool {
    matches!(frame_type, 0x02 | 0x06 | 0x08 | 0x09)
}

impl H3Frame {
    /// Encode this frame into the given buffer.
    pub fn encode(&self, buf: &mut Vec<u8>) -> Result<(), HttpError> {
        match self {
            Self::Data(data) => {
                encode_varint(0x00, buf).map_err(HttpError::Quic)?;
                encode_varint(data.len() as u64, buf).map_err(HttpError::Quic)?;
                buf.extend_from_slice(data);
            }
            Self::Headers(data) => {
                encode_varint(0x01, buf).map_err(HttpError::Quic)?;
                encode_varint(data.len() as u64, buf).map_err(HttpError::Quic)?;
                buf.extend_from_slice(data);
            }
            Self::CancelPush(push_id) => {
                encode_varint(0x03, buf).map_err(HttpError::Quic)?;
                let mut payload = Vec::new();
                encode_varint(*push_id, &mut payload).map_err(HttpError::Quic)?;
                encode_varint(payload.len() as u64, buf).map_err(HttpError::Quic)?;
                buf.extend_from_slice(&payload);
            }
            Self::Settings(settings) => {
                encode_varint(0x04, buf).map_err(HttpError::Quic)?;
                let mut payload = Vec::new();
                for s in settings {
                    encode_varint(s.id, &mut payload).map_err(HttpError::Quic)?;
                    encode_varint(s.value, &mut payload).map_err(HttpError::Quic)?;
                }
                encode_varint(payload.len() as u64, buf).map_err(HttpError::Quic)?;
                buf.extend_from_slice(&payload);
            }
            Self::PushPromise { push_id, headers } => {
                encode_varint(0x05, buf).map_err(HttpError::Quic)?;
                let mut payload = Vec::new();
                encode_varint(*push_id, &mut payload).map_err(HttpError::Quic)?;
                payload.extend_from_slice(headers);
                encode_varint(payload.len() as u64, buf).map_err(HttpError::Quic)?;
                buf.extend_from_slice(&payload);
            }
            Self::GoAway(id) => {
                encode_varint(0x07, buf).map_err(HttpError::Quic)?;
                let mut payload = Vec::new();
                encode_varint(*id, &mut payload).map_err(HttpError::Quic)?;
                encode_varint(payload.len() as u64, buf).map_err(HttpError::Quic)?;
                buf.extend_from_slice(&payload);
            }
            Self::MaxPushId(id) => {
                encode_varint(0x0d, buf).map_err(HttpError::Quic)?;
                let mut payload = Vec::new();
                encode_varint(*id, &mut payload).map_err(HttpError::Quic)?;
                encode_varint(payload.len() as u64, buf).map_err(HttpError::Quic)?;
                buf.extend_from_slice(&payload);
            }
            Self::PriorityUpdateRequest { stream_id, value } => {
                encode_varint(0x0f0700, buf).map_err(HttpError::Quic)?;
                let mut payload = Vec::new();
                encode_varint(*stream_id, &mut payload).map_err(HttpError::Quic)?;
                payload.extend_from_slice(value);
                encode_varint(payload.len() as u64, buf).map_err(HttpError::Quic)?;
                buf.extend_from_slice(&payload);
            }
            Self::PriorityUpdatePush { push_id, value } => {
                encode_varint(0x0f0701, buf).map_err(HttpError::Quic)?;
                let mut payload = Vec::new();
                encode_varint(*push_id, &mut payload).map_err(HttpError::Quic)?;
                payload.extend_from_slice(value);
                encode_varint(payload.len() as u64, buf).map_err(HttpError::Quic)?;
                buf.extend_from_slice(&payload);
            }
            Self::Unknown { frame_type, data } => {
                encode_varint(*frame_type, buf).map_err(HttpError::Quic)?;
                encode_varint(data.len() as u64, buf).map_err(HttpError::Quic)?;
                buf.extend_from_slice(data);
            }
        }

        Ok(())
    }

    /// Decode a single frame from the buffer.
    ///
    /// Returns `(frame, bytes_consumed)` or an error.
    /// Returns `Ok(None)` if there aren't enough bytes yet.
    pub fn decode(buf: &[u8]) -> Result<Option<(Self, usize)>, HttpError> {
        let mut pos = 0;

        let (frame_type, n) = match decode_varint(&buf[pos..]) {
            Some(v) => v,
            None => return Ok(None),
        };
        pos += n;

        let (length, n) = match decode_varint(&buf[pos..]) {
            Some(v) => v,
            None => return Ok(None),
        };
        pos += n;

        let length = usize::try_from(length)
            .map_err(|_| HttpError::Parse("HTTP/3 frame length exceeds platform usize".into()))?;
        if buf.len() - pos < length {
            return Ok(None); // need more data
        }

        let payload = &buf[pos..pos + length];
        pos += length;

        let frame = match frame_type {
            frame_type if is_reserved_http2_frame_type(frame_type) => {
                return Err(HttpError::Parse(format!(
                    "H3_FRAME_UNEXPECTED (0x{:04x}): reserved HTTP/2 frame type 0x{frame_type:02x}",
                    crate::h3::error::H3_FRAME_UNEXPECTED,
                )));
            }
            0x00 => Self::Data(payload.to_vec()),
            0x01 => Self::Headers(payload.to_vec()),
            0x03 => Self::CancelPush(decode_exact_varint_payload(payload, "CANCEL_PUSH")?),
            0x04 => {
                let mut settings = Vec::new();
                let mut sp = 0;
                while sp < payload.len() {
                    let (id, n) = decode_varint(&payload[sp..])
                        .ok_or_else(|| HttpError::Parse("truncated SETTINGS id".into()))?;
                    sp += n;
                    let (value, n) = decode_varint(&payload[sp..])
                        .ok_or_else(|| HttpError::Parse("truncated SETTINGS value".into()))?;
                    sp += n;
                    settings.push(H3Setting { id, value });
                }
                Self::Settings(settings)
            }
            0x05 => {
                let (push_id, n) = decode_varint(payload)
                    .ok_or_else(|| HttpError::Parse("truncated PUSH_PROMISE".into()))?;
                Self::PushPromise {
                    push_id,
                    headers: payload[n..].to_vec(),
                }
            }
            0x07 => Self::GoAway(decode_exact_varint_payload(payload, "GOAWAY")?),
            0x0d => Self::MaxPushId(decode_exact_varint_payload(payload, "MAX_PUSH_ID")?),
            0x0f0700 => {
                let (stream_id, n) = decode_varint(payload)
                    .ok_or_else(|| HttpError::Parse("truncated PRIORITY_UPDATE request".into()))?;
                Self::PriorityUpdateRequest {
                    stream_id,
                    value: payload[n..].to_vec(),
                }
            }
            0x0f0701 => {
                let (push_id, n) = decode_varint(payload)
                    .ok_or_else(|| HttpError::Parse("truncated PRIORITY_UPDATE push".into()))?;
                Self::PriorityUpdatePush {
                    push_id,
                    value: payload[n..].to_vec(),
                }
            }
            _ => Self::Unknown {
                frame_type,
                data: payload.to_vec(),
            },
        };

        Ok(Some((frame, pos)))
    }
}

/// Decode all frames from a stream that has reached FIN.
///
/// Returns an error if the buffer ends with a partial frame instead of a
/// complete frame boundary.
pub(crate) fn decode_all_frames_at_eof(buf: &[u8]) -> Result<Vec<H3Frame>, HttpError> {
    let mut frames = Vec::new();
    let mut pos = 0;

    while pos < buf.len() {
        match H3Frame::decode(&buf[pos..])? {
            Some((frame, consumed)) => {
                frames.push(frame);
                pos += consumed;
            }
            None => {
                return Err(HttpError::Parse(
                    "truncated HTTP/3 frame at end of stream".into(),
                ));
            }
        }
    }

    Ok(frames)
}

// ─── Variable-length integer decoding ────────────────────────────────
//
// Encoding uses `starfish_quic::packet::encode_varint` directly (re-imported above).
// Decoding wraps the QUIC version to return `Option` instead of `Result`.

/// Decode a variable-length integer. Returns `(value, bytes_consumed)` or `None`.
fn decode_varint(buf: &[u8]) -> Option<(u64, usize)> {
    quic_decode_varint(buf).ok()
}

fn decode_exact_varint_payload(payload: &[u8], label: &str) -> Result<u64, HttpError> {
    let (value, consumed) =
        decode_varint(payload).ok_or_else(|| HttpError::Parse(format!("truncated {label}")))?;
    if consumed != payload.len() {
        return Err(HttpError::Parse(format!(
            "H3_FRAME_ERROR (0x{:04x}): {label} frame has trailing bytes",
            crate::h3::error::H3_FRAME_ERROR,
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for &v in &[0u64, 37, 63, 64, 16383, 16384, 1073741823, 1073741824] {
            let mut buf = Vec::new();
            encode_varint(v, &mut buf).unwrap();
            let (decoded, len) = decode_varint(&buf).unwrap();
            assert_eq!(decoded, v);
            assert_eq!(len, buf.len());
        }
    }

    #[test]
    fn data_frame_roundtrip() {
        let frame = H3Frame::Data(b"hello".to_vec());
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();

        let (decoded, consumed) = H3Frame::decode(&buf).unwrap().unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(decoded, frame);
    }

    #[test]
    fn headers_frame_roundtrip() {
        let frame = H3Frame::Headers(vec![1, 2, 3, 4]);
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();

        let (decoded, _) = H3Frame::decode(&buf).unwrap().unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn settings_frame_roundtrip() {
        let frame = H3Frame::Settings(vec![
            H3Setting {
                id: setting_id::QPACK_MAX_TABLE_CAPACITY,
                value: 4096,
            },
            H3Setting {
                id: setting_id::MAX_FIELD_SECTION_SIZE,
                value: 16384,
            },
        ]);
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();

        let (decoded, _) = H3Frame::decode(&buf).unwrap().unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn goaway_frame_roundtrip() {
        let frame = H3Frame::GoAway(42);
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();

        let (decoded, _) = H3Frame::decode(&buf).unwrap().unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn priority_update_request_roundtrip() {
        let frame = H3Frame::PriorityUpdateRequest {
            stream_id: 9,
            value: b"u=1, i".to_vec(),
        };
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();

        let (decoded, _) = H3Frame::decode(&buf).unwrap().unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn incomplete_returns_none() {
        let frame = H3Frame::Data(b"test".to_vec());
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();

        // Feed only part of the frame
        assert!(H3Frame::decode(&buf[..2]).unwrap().is_none());
    }

    #[test]
    fn decode_all_frames_at_eof_rejects_truncated_frame() {
        let frame = H3Frame::Data(b"hello".to_vec());
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        buf.pop();

        let err = decode_all_frames_at_eof(&buf).unwrap_err();
        assert!(matches!(
            err,
            HttpError::Parse(message) if message.contains("truncated HTTP/3 frame")
        ));
    }

    #[test]
    fn unknown_frame_type() {
        let mut buf = Vec::new();
        encode_varint(0xff, &mut buf).unwrap(); // unknown type
        encode_varint(3, &mut buf).unwrap();
        buf.extend_from_slice(b"abc");

        let (decoded, _) = H3Frame::decode(&buf).unwrap().unwrap();
        match decoded {
            H3Frame::Unknown { frame_type, data } => {
                assert_eq!(frame_type, 0xff);
                assert_eq!(data, b"abc");
            }
            _ => panic!("expected Unknown frame"),
        }
    }

    #[test]
    fn reserved_http2_frame_type_is_rejected() {
        let mut buf = Vec::new();
        encode_varint(0x06, &mut buf).unwrap();
        encode_varint(0, &mut buf).unwrap();

        let err = H3Frame::decode(&buf).unwrap_err();
        assert!(
            matches!(err, HttpError::Parse(message) if message.contains("H3_FRAME_UNEXPECTED"))
        );
    }

    #[test]
    fn cancel_push_rejects_trailing_bytes() {
        let mut buf = Vec::new();
        encode_varint(0x03, &mut buf).unwrap();
        let mut payload = Vec::new();
        encode_varint(7, &mut payload).unwrap();
        payload.push(0);
        encode_varint(payload.len() as u64, &mut buf).unwrap();
        buf.extend_from_slice(&payload);

        let err = H3Frame::decode(&buf).unwrap_err();
        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_FRAME_ERROR")));
    }

    #[test]
    fn goaway_rejects_trailing_bytes() {
        let mut buf = Vec::new();
        encode_varint(0x07, &mut buf).unwrap();
        let mut payload = Vec::new();
        encode_varint(4, &mut payload).unwrap();
        payload.push(0);
        encode_varint(payload.len() as u64, &mut buf).unwrap();
        buf.extend_from_slice(&payload);

        let err = H3Frame::decode(&buf).unwrap_err();
        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_FRAME_ERROR")));
    }

    #[test]
    fn max_push_id_rejects_trailing_bytes() {
        let mut buf = Vec::new();
        encode_varint(0x0d, &mut buf).unwrap();
        let mut payload = Vec::new();
        encode_varint(9, &mut payload).unwrap();
        payload.push(0);
        encode_varint(payload.len() as u64, &mut buf).unwrap();
        buf.extend_from_slice(&payload);

        let err = H3Frame::decode(&buf).unwrap_err();
        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_FRAME_ERROR")));
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn decode_rejects_frame_length_larger_than_usize() {
        let mut buf = Vec::new();
        encode_varint(0x00, &mut buf).unwrap();
        encode_varint(u32::MAX as u64 + 1, &mut buf).unwrap();

        let err = H3Frame::decode(&buf).unwrap_err();
        assert!(matches!(
            err,
            HttpError::Parse(message) if message.contains("frame length exceeds platform usize")
        ));
    }
}
