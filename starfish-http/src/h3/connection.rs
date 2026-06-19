//! HTTP/3 connection management (RFC 9114 §3).
//!
//! [`H3Connection`] wraps a [`QuicConnection`] and manages the HTTP/3
//! control streams and SETTINGS exchange.

use std::collections::{HashMap, HashSet, VecDeque};

use starfish_quic::{decode_varint, encode_varint, QuicConnection};

use super::error;
use super::frame::{decode_all_frames_at_eof, is_reserved_http2_setting_id, H3Frame, H3Setting};
use super::qpack::decoder::QpackDecoder;
use super::qpack::encoder::QpackEncoder;
use crate::error::HttpError;

const CONTROL_STREAM_TYPE: u64 = 0x00;
const PUSH_STREAM_TYPE: u64 = 0x01;
const QPACK_ENCODER_STREAM_TYPE: u64 = 0x02;
const QPACK_DECODER_STREAM_TYPE: u64 = 0x03;
const LOCAL_QPACK_MAX_TABLE_CAPACITY: u64 = 0;
const LOCAL_QPACK_BLOCKED_STREAMS: u64 = 0;
pub(crate) const LOCAL_MAX_PUSH_ID: u64 = 16;
const MAX_UNI_STREAM_TYPE_BYTES: usize = 8;
const MAX_CONTROL_STREAM_PENDING_BYTES: usize = 64 * 1024;
const MAX_PUSH_STREAM_PENDING_BYTES: usize = 1024 * 1024;
const MAX_PUSH_STREAM_BODY_BYTES: usize = 1024 * 1024;

/// A server-pushed response received by an HTTP/3 client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H3Push {
    pub push_id: u64,
    pub request_headers: Vec<(String, String)>,
    pub response_headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// HTTP/3 connection wrapping a QUIC connection.
pub struct H3Connection {
    /// Underlying QUIC connection.
    pub(crate) quic: QuicConnection,
    /// QPACK encoder for outgoing headers.
    pub(crate) encoder: QpackEncoder,
    /// QPACK decoder for incoming headers.
    pub(crate) decoder: QpackDecoder,
    /// Peer's settings, once received.
    pub(crate) peer_settings: Option<Vec<H3Setting>>,
    /// Whether the local control/QPACK streams have been opened.
    pub(crate) control_stream_opened: bool,
    /// Stream ID of the local control stream, once opened.
    local_control_stream_id: Option<u64>,
    /// Stream ID of the local QPACK encoder stream, once opened.
    local_qpack_encoder_stream_id: Option<u64>,
    /// Stream ID of the local QPACK decoder stream, once opened.
    local_qpack_decoder_stream_id: Option<u64>,
    /// Incremental parse state for incoming unidirectional streams.
    uni_stream_states: HashMap<u64, UniStreamState>,
    /// Stream ID of the peer's control stream, once identified.
    peer_control_stream_id: Option<u64>,
    /// Stream ID of the peer's QPACK encoder stream, once identified.
    peer_qpack_encoder_stream_id: Option<u64>,
    /// Stream ID of the peer's QPACK decoder stream, once identified.
    peer_qpack_decoder_stream_id: Option<u64>,
    /// Whether a GOAWAY frame has been received from the peer.
    goaway_received: bool,
    /// The last stream ID that the peer will process (from GOAWAY).
    /// Requests on streams with IDs above this should not be initiated.
    goaway_last_stream_id: Option<u64>,
    /// Maximum push ID the peer has permitted us to use.
    peer_max_push_id: u64,
    /// Maximum push ID we have advertised to the peer, if any.
    local_max_push_id: Option<u64>,
    /// Next locally-generated push ID.
    next_local_push_id: u64,
    /// Promised push requests keyed by push ID until the push stream arrives.
    promised_pushes: HashMap<u64, Vec<(String, String)>>,
    /// Completed pushed responses ready for the caller.
    ready_pushes: VecDeque<H3Push>,
}

#[derive(Debug, Default)]
struct UniStreamState {
    stream_type: Option<u64>,
    push_id: Option<u64>,
    pending: Vec<u8>,
    /// Whether we have received a SETTINGS frame on this control stream.
    settings_seen: bool,
    push_response_headers_block: Option<Vec<u8>>,
    push_trailers_block: Option<Vec<u8>>,
    push_body: Vec<u8>,
}

impl UniStreamState {
    fn ingest(&mut self, chunk: &[u8], fin: bool) -> Result<(), HttpError> {
        self.pending.extend_from_slice(chunk);

        if self.stream_type.is_none() {
            match decode_varint(&self.pending) {
                Ok((stream_type, consumed)) => {
                    self.stream_type = Some(stream_type);
                    self.pending.drain(..consumed);
                }
                Err(_) if fin => {
                    return Err(HttpError::Parse(
                        "truncated unidirectional stream type".into(),
                    ));
                }
                Err(_) => {
                    if self.pending.len() > MAX_UNI_STREAM_TYPE_BYTES {
                        return Err(HttpError::Parse(format!(
                            "H3_EXCESSIVE_LOAD (0x{:04x}): unidirectional stream type \
                             exceeded {} bytes",
                            error::H3_EXCESSIVE_LOAD,
                            MAX_UNI_STREAM_TYPE_BYTES,
                        )));
                    }
                    return Ok(());
                }
            }
        }

        self.ensure_pending_limit()?;
        Ok(())
    }

    fn stream_type(&self) -> Option<u64> {
        self.stream_type
    }

    fn ingest_push_id(&mut self, fin: bool) -> Result<Option<u64>, HttpError> {
        if self.push_id.is_some() {
            return Ok(self.push_id);
        }

        match decode_varint(&self.pending) {
            Ok((push_id, consumed)) => {
                self.push_id = Some(push_id);
                self.pending.drain(..consumed);
                Ok(self.push_id)
            }
            Err(_) if fin => Err(HttpError::Parse("truncated push stream push ID".into())),
            Err(_) => Ok(None),
        }
    }

    fn ingest_control_frames(&mut self, fin: bool) -> Result<Vec<H3Frame>, HttpError> {
        let mut frames = Vec::new();
        loop {
            match H3Frame::decode(&self.pending)? {
                Some((frame, consumed)) => {
                    self.pending.drain(..consumed);

                    // RFC 9114 §6.2.1: First frame on control stream MUST be SETTINGS
                    if !self.settings_seen {
                        match &frame {
                            H3Frame::Settings(_) => {
                                self.settings_seen = true;
                            }
                            _ => {
                                return Err(HttpError::Parse(format!(
                                    "H3_MISSING_SETTINGS (0x{:04x}): first frame on control stream must be SETTINGS, got {:?}",
                                    error::H3_MISSING_SETTINGS,
                                    std::mem::discriminant(&frame),
                                )));
                            }
                        }
                    } else if matches!(frame, H3Frame::Settings(_)) {
                        return Err(h3_frame_unexpected(
                            "duplicate SETTINGS frame on control stream",
                        ));
                    }

                    // RFC 9114 §7.2.4: Reject frames not allowed on control stream
                    match &frame {
                        H3Frame::Data(_) | H3Frame::Headers(_) | H3Frame::PushPromise { .. } => {
                            return Err(HttpError::Parse(format!(
                                "H3_FRAME_UNEXPECTED (0x{:04x}): frame type not \
                                 allowed on control stream",
                                error::H3_FRAME_UNEXPECTED,
                            )));
                        }
                        _ => {}
                    }

                    frames.push(frame);
                }
                None if fin && !self.pending.is_empty() => {
                    return Err(HttpError::Parse("truncated control stream frame".into()));
                }
                None => break,
            }
        }

        self.ensure_pending_limit()?;
        Ok(frames)
    }

    fn ingest_push_frames(&mut self, fin: bool) -> Result<(), HttpError> {
        loop {
            match H3Frame::decode(&self.pending)? {
                Some((frame, consumed)) => {
                    self.pending.drain(..consumed);
                    match frame {
                        H3Frame::Headers(block) => {
                            if self.push_response_headers_block.is_none() {
                                self.push_response_headers_block = Some(block);
                            } else if self.push_trailers_block.is_none() {
                                self.push_trailers_block = Some(block);
                            } else {
                                return Err(HttpError::Parse(format!(
                                    "H3_FRAME_UNEXPECTED (0x{:04x}): \
                                     more than two HEADERS frames on push stream",
                                    error::H3_FRAME_UNEXPECTED,
                                )));
                            }
                        }
                        H3Frame::Data(data) => {
                            if self.push_response_headers_block.is_none() {
                                return Err(HttpError::Parse(format!(
                                    "H3_FRAME_UNEXPECTED (0x{:04x}): \
                                     DATA before HEADERS on push stream",
                                    error::H3_FRAME_UNEXPECTED,
                                )));
                            }
                            if self.push_trailers_block.is_some() {
                                return Err(HttpError::Parse(format!(
                                    "H3_FRAME_UNEXPECTED (0x{:04x}): \
                                     DATA after trailers on push stream",
                                    error::H3_FRAME_UNEXPECTED,
                                )));
                            }
                            self.ensure_push_body_capacity(data.len())?;
                            self.push_body.extend_from_slice(&data);
                        }
                        H3Frame::PushPromise { .. }
                        | H3Frame::Settings(_)
                        | H3Frame::GoAway(_)
                        | H3Frame::MaxPushId(_)
                        | H3Frame::CancelPush(_)
                        | H3Frame::PriorityUpdateRequest { .. }
                        | H3Frame::PriorityUpdatePush { .. } => {
                            return Err(HttpError::Parse(format!(
                                "H3_FRAME_UNEXPECTED (0x{:04x}): \
                                 invalid frame on push stream",
                                error::H3_FRAME_UNEXPECTED,
                            )));
                        }
                        H3Frame::Unknown { .. } => {}
                    }
                }
                None if fin && !self.pending.is_empty() => {
                    return Err(HttpError::Parse("truncated push stream frame".into()));
                }
                None => break,
            }
        }

        self.ensure_pending_limit()?;
        Ok(())
    }

    fn ensure_push_body_capacity(&self, additional: usize) -> Result<(), HttpError> {
        let total = self.push_body.len().saturating_add(additional);
        if total > MAX_PUSH_STREAM_BODY_BYTES {
            return Err(HttpError::Parse(format!(
                "H3_EXCESSIVE_LOAD (0x{:04x}): push response body buffered \
                 {total} bytes (limit {MAX_PUSH_STREAM_BODY_BYTES})",
                error::H3_EXCESSIVE_LOAD,
            )));
        }

        Ok(())
    }

    fn ensure_pending_limit(&self) -> Result<(), HttpError> {
        let (limit, label) = match self.stream_type {
            Some(CONTROL_STREAM_TYPE) => (MAX_CONTROL_STREAM_PENDING_BYTES, "control"),
            Some(PUSH_STREAM_TYPE) => (MAX_PUSH_STREAM_PENDING_BYTES, "push"),
            Some(_) => return Ok(()),
            None => (MAX_UNI_STREAM_TYPE_BYTES, "untyped"),
        };

        if self.pending.len() > limit {
            return Err(HttpError::Parse(format!(
                "H3_EXCESSIVE_LOAD (0x{:04x}): {label} unidirectional stream buffered \
                 {} bytes (limit {limit})",
                error::H3_EXCESSIVE_LOAD,
                self.pending.len(),
            )));
        }

        Ok(())
    }
}

fn h3_frame_unexpected(message: impl Into<String>) -> HttpError {
    HttpError::Parse(format!(
        "H3_FRAME_UNEXPECTED (0x{:04x}): {}",
        error::H3_FRAME_UNEXPECTED,
        message.into(),
    ))
}

fn h3_message_error(message: impl Into<String>) -> HttpError {
    HttpError::Parse(format!(
        "H3_MESSAGE_ERROR (0x{:04x}): {}",
        error::H3_MESSAGE_ERROR,
        message.into(),
    ))
}

fn h3_id_error(message: impl Into<String>) -> HttpError {
    HttpError::Parse(format!(
        "H3_ID_ERROR (0x{:04x}): {}",
        error::H3_ID_ERROR,
        message.into(),
    ))
}

fn validate_local_max_push_id(current: Option<u64>, max_push_id: u64) -> Result<(), HttpError> {
    if let Some(previous) = current {
        if max_push_id < previous {
            return Err(h3_id_error(format!(
                "locally advertised MAX_PUSH_ID decreased from {} to {}",
                previous, max_push_id,
            )));
        }
    }
    Ok(())
}

fn h3_settings_error(message: impl Into<String>) -> HttpError {
    HttpError::Parse(format!(
        "H3_SETTINGS_ERROR (0x{:04x}): {}",
        error::H3_SETTINGS_ERROR,
        message.into(),
    ))
}

fn reject_server_max_push_id(max_push_id: u64) -> Result<(), HttpError> {
    Err(h3_frame_unexpected(format!(
        "server sent MAX_PUSH_ID {max_push_id}",
    )))
}

fn ensure_initial_headers_seen(
    saw_initial_headers: bool,
    stream_label: &str,
) -> Result<(), HttpError> {
    if !saw_initial_headers {
        return Err(h3_message_error(format!(
            "{stream_label} stream ended without an initial HEADERS frame",
        )));
    }

    Ok(())
}

fn validate_incoming_push_id(
    local_max_push_id: Option<u64>,
    push_id: u64,
) -> Result<(), HttpError> {
    match local_max_push_id {
        Some(max_push_id) if push_id <= max_push_id => Ok(()),
        Some(max_push_id) => Err(h3_id_error(format!(
            "received push ID {push_id} above advertised MAX_PUSH_ID {max_push_id}",
        ))),
        None => Err(h3_id_error(format!(
            "received push ID {push_id} before advertising MAX_PUSH_ID",
        ))),
    }
}

/// Validate that settings contain no duplicate identifiers.
///
/// RFC 9114 §7.2.4.1: duplicate setting identifiers MUST be treated
/// as a connection error of type H3_SETTINGS_ERROR.
fn validate_settings(settings: &[H3Setting]) -> Result<(), HttpError> {
    let mut seen_ids = HashSet::new();
    for s in settings {
        if is_reserved_http2_setting_id(s.id) {
            return Err(h3_settings_error(format!(
                "reserved HTTP/2 setting id 0x{:02x}",
                s.id,
            )));
        }
        if !seen_ids.insert(s.id) {
            return Err(h3_settings_error(format!(
                "duplicate setting id 0x{:02x}",
                s.id,
            )));
        }
    }
    Ok(())
}

impl H3Connection {
    /// Create a new H3Connection from an established QUIC connection.
    pub fn new(quic: QuicConnection) -> Self {
        Self {
            quic,
            encoder: QpackEncoder::new(),
            decoder: QpackDecoder::with_capacity(
                LOCAL_QPACK_MAX_TABLE_CAPACITY as usize,
                LOCAL_QPACK_BLOCKED_STREAMS as usize,
            ),
            peer_settings: None,
            control_stream_opened: false,
            local_control_stream_id: None,
            local_qpack_encoder_stream_id: None,
            local_qpack_decoder_stream_id: None,
            uni_stream_states: HashMap::new(),
            peer_control_stream_id: None,
            peer_qpack_encoder_stream_id: None,
            peer_qpack_decoder_stream_id: None,
            goaway_received: false,
            goaway_last_stream_id: None,
            peer_max_push_id: 0,
            local_max_push_id: None,
            next_local_push_id: 0,
            promised_pushes: HashMap::new(),
            ready_pushes: VecDeque::new(),
        }
    }

    /// Send the initial SETTINGS frame and open the QPACK streams.
    pub async fn open_control_stream(&mut self) -> Result<(), HttpError> {
        if self.control_stream_opened {
            return Ok(());
        }

        let control_stream_id = self.quic.open_uni_stream().map_err(HttpError::Quic)?;
        let encoder_stream_id = self.quic.open_uni_stream().map_err(HttpError::Quic)?;
        let decoder_stream_id = self.quic.open_uni_stream().map_err(HttpError::Quic)?;

        let mut control_buf = Vec::new();
        encode_varint(CONTROL_STREAM_TYPE, &mut control_buf).map_err(HttpError::Quic)?;

        let settings = H3Frame::Settings(vec![
            H3Setting {
                id: super::frame::setting_id::QPACK_MAX_TABLE_CAPACITY,
                value: LOCAL_QPACK_MAX_TABLE_CAPACITY,
            },
            H3Setting {
                id: super::frame::setting_id::QPACK_BLOCKED_STREAMS,
                value: LOCAL_QPACK_BLOCKED_STREAMS,
            },
        ]);
        settings.encode(&mut control_buf)?;

        self.quic
            .stream_send(control_stream_id, &control_buf, false)
            .map_err(HttpError::Quic)?;

        let mut encoder_buf = Vec::new();
        encode_varint(QPACK_ENCODER_STREAM_TYPE, &mut encoder_buf).map_err(HttpError::Quic)?;
        self.quic
            .stream_send(encoder_stream_id, &encoder_buf, false)
            .map_err(HttpError::Quic)?;

        let mut decoder_buf = Vec::new();
        encode_varint(QPACK_DECODER_STREAM_TYPE, &mut decoder_buf).map_err(HttpError::Quic)?;
        self.quic
            .stream_send(decoder_stream_id, &decoder_buf, false)
            .map_err(HttpError::Quic)?;

        self.quic.flush().await.map_err(HttpError::Quic)?;
        self.control_stream_opened = true;
        self.local_control_stream_id = Some(control_stream_id);
        self.local_qpack_encoder_stream_id = Some(encoder_stream_id);
        self.local_qpack_decoder_stream_id = Some(decoder_stream_id);
        Ok(())
    }

    /// Drive QUIC I/O and then process any HTTP/3 unidirectional streams.
    pub async fn poll(&mut self) -> Result<(), HttpError> {
        self.quic.poll().await.map_err(HttpError::Quic)?;
        self.poll_control().await
    }

    async fn flush_pending(&mut self) -> Result<(), HttpError> {
        self.quic.flush().await.map_err(HttpError::Quic)?;
        self.poll_control().await
    }

    /// Send an HTTP request on a new bidirectional stream.
    ///
    /// Encodes the headers with QPACK, sends HEADERS + optional DATA frames.
    /// Returns the stream ID used.
    ///
    /// Returns an error if the peer has sent a GOAWAY frame — no new requests
    /// should be initiated after receiving GOAWAY.
    pub async fn send_request(
        &mut self,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<u64, HttpError> {
        self.open_control_stream().await?;
        self.poll_control().await?;

        if self.goaway_received {
            return Err(HttpError::ConnectionClosed);
        }
        let stream_id = self.quic.open_bidi_stream().map_err(HttpError::Quic)?;

        let mut buf = Vec::new();

        let header_block = self.encoder.encode_header_block(stream_id, headers)?;
        let headers_frame = H3Frame::Headers(header_block);
        headers_frame.encode(&mut buf)?;

        // Optionally send DATA frame
        if let Some(body_data) = body {
            if !body_data.is_empty() {
                let data_frame = H3Frame::Data(body_data.to_vec());
                data_frame.encode(&mut buf)?;
            }
        }

        self.quic
            .stream_send(stream_id, &buf, true) // FIN after request
            .map_err(HttpError::Quic)?;

        self.flush_qpack_stream_data().await?;
        self.flush_pending().await?;
        Ok(stream_id)
    }

    /// Read a response from a request stream.
    ///
    /// Reads HEADERS and DATA frames from the given stream and returns
    /// the decoded headers and body.
    pub async fn read_response(
        &mut self,
        stream_id: u64,
    ) -> Result<(Vec<(String, String)>, Vec<u8>), HttpError> {
        self.poll_control().await?;

        let mut recv_buf = vec![0u8; 16384];
        let mut accumulated = Vec::new();
        let max_empty_polls = 1000;
        let mut empty_polls = 0;

        loop {
            match self.quic.stream_recv(stream_id, &mut recv_buf) {
                Ok((n, fin)) => {
                    if n > 0 {
                        accumulated.extend_from_slice(&recv_buf[..n]);
                        empty_polls = 0; // reset on progress
                    }
                    if fin {
                        break;
                    }
                    if n == 0 {
                        empty_polls += 1;
                        if empty_polls >= max_empty_polls {
                            return Err(HttpError::Parse(
                                "read_response: no progress after repeated polls".into(),
                            ));
                        }
                        self.poll().await?;
                    }
                }
                Err(e) => return Err(HttpError::Quic(e)),
            }
        }

        // Decode frames with request-stream validation (RFC 9114 §4.1):
        //   HEADERS (initial) → DATA* → HEADERS (trailers, optional)
        // Control frames are not allowed on request streams.
        let mut headers = Vec::new();
        let mut body = Vec::new();
        let mut saw_initial_headers = false;
        let mut saw_trailers = false;

        for frame in decode_all_frames_at_eof(&accumulated)? {
            match frame {
                H3Frame::Headers(block) => {
                    if !saw_initial_headers {
                        headers = self.decode_header_block_wait(stream_id, &block).await?;
                        saw_initial_headers = true;
                    } else if !saw_trailers {
                        // Trailers — decode but don't overwrite initial headers
                        let _trailers = self.decode_header_block_wait(stream_id, &block).await?;
                        saw_trailers = true;
                    } else {
                        return Err(HttpError::Parse(format!(
                            "H3_FRAME_UNEXPECTED (0x{:04x}): \
                             more than two HEADERS frames on request stream",
                            error::H3_FRAME_UNEXPECTED,
                        )));
                    }
                }
                H3Frame::Data(data) => {
                    if !saw_initial_headers {
                        return Err(HttpError::Parse(format!(
                            "H3_FRAME_UNEXPECTED (0x{:04x}): \
                             DATA before HEADERS on request stream",
                            error::H3_FRAME_UNEXPECTED,
                        )));
                    }
                    if saw_trailers {
                        return Err(HttpError::Parse(format!(
                            "H3_FRAME_UNEXPECTED (0x{:04x}): \
                             DATA after trailers on request stream",
                            error::H3_FRAME_UNEXPECTED,
                        )));
                    }
                    body.extend_from_slice(&data);
                }
                H3Frame::Settings(_)
                | H3Frame::GoAway(_)
                | H3Frame::MaxPushId(_)
                | H3Frame::CancelPush(_)
                | H3Frame::PriorityUpdateRequest { .. }
                | H3Frame::PriorityUpdatePush { .. } => {
                    return Err(HttpError::Parse(format!(
                        "H3_FRAME_UNEXPECTED (0x{:04x}): \
                         control frame on request stream",
                        error::H3_FRAME_UNEXPECTED,
                    )));
                }
                H3Frame::PushPromise {
                    push_id,
                    headers: block,
                } => {
                    let promised = self.decode_header_block_wait(stream_id, &block).await?;
                    self.store_promised_push(push_id, promised)?;
                }
                H3Frame::Unknown { .. } => {
                    // Unknown frame types MUST be ignored (RFC 9114 §9)
                }
            }
        }

        ensure_initial_headers_seen(saw_initial_headers, "request/response")?;
        Ok((headers, body))
    }

    /// Read a request from a stream (server side).
    pub async fn read_request(
        &mut self,
        stream_id: u64,
    ) -> Result<(Vec<(String, String)>, Vec<u8>), HttpError> {
        // Same as read_response — the frame structure is identical
        self.read_response(stream_id).await
    }

    /// Send a response on a request stream (server side).
    pub async fn send_response(
        &mut self,
        stream_id: u64,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(), HttpError> {
        self.open_control_stream().await?;
        self.poll_control().await?;

        let mut buf = Vec::new();

        let header_block = self.encoder.encode_header_block(stream_id, headers)?;
        let headers_frame = H3Frame::Headers(header_block);
        headers_frame.encode(&mut buf)?;

        if let Some(body_data) = body {
            if !body_data.is_empty() {
                let data_frame = H3Frame::Data(body_data.to_vec());
                data_frame.encode(&mut buf)?;
            }
        }

        self.quic
            .stream_send(stream_id, &buf, true)
            .map_err(HttpError::Quic)?;

        self.flush_qpack_stream_data().await?;
        self.flush_pending().await?;
        Ok(())
    }

    /// Send a PRIORITY_UPDATE frame for a request stream on the control stream.
    pub async fn send_priority_update(
        &mut self,
        stream_id: u64,
        value: &[u8],
    ) -> Result<(), HttpError> {
        self.open_control_stream().await?;
        let control_stream_id = self
            .local_control_stream_id
            .ok_or_else(|| HttpError::Parse("local control stream missing".into()))?;

        let mut buf = Vec::new();
        H3Frame::PriorityUpdateRequest {
            stream_id,
            value: value.to_vec(),
        }
        .encode(&mut buf)?;
        self.quic
            .stream_send(control_stream_id, &buf, false)
            .map_err(HttpError::Quic)?;
        self.flush_pending().await
    }

    /// Advertise the maximum push ID the peer may use for server push.
    pub async fn send_max_push_id(&mut self, max_push_id: u64) -> Result<(), HttpError> {
        validate_local_max_push_id(self.local_max_push_id, max_push_id)?;
        self.open_control_stream().await?;
        let control_stream_id = self
            .local_control_stream_id
            .ok_or_else(|| HttpError::Parse("local control stream missing".into()))?;

        let mut buf = Vec::new();
        H3Frame::MaxPushId(max_push_id).encode(&mut buf)?;
        self.quic
            .stream_send(control_stream_id, &buf, false)
            .map_err(HttpError::Quic)?;
        self.local_max_push_id = Some(max_push_id);
        self.flush_pending().await
    }

    /// Send a server push promise plus push stream for a pushed response.
    pub async fn send_push(
        &mut self,
        parent_stream_id: u64,
        request_headers: &[(&str, &str)],
        response_headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<u64, HttpError> {
        self.open_control_stream().await?;
        self.poll_control().await?;

        if !push_id_within_limit(self.next_local_push_id, self.peer_max_push_id) {
            return Err(HttpError::Parse(
                "peer did not permit another server push".into(),
            ));
        }
        let push_id = self.next_local_push_id;
        self.next_local_push_id += 1;

        let request_block = self
            .encoder
            .encode_header_block(parent_stream_id, request_headers)?;
        let mut promise_buf = Vec::new();
        H3Frame::PushPromise {
            push_id,
            headers: request_block,
        }
        .encode(&mut promise_buf)?;
        self.quic
            .stream_send(parent_stream_id, &promise_buf, false)
            .map_err(HttpError::Quic)?;

        let push_stream_id = self.quic.open_uni_stream().map_err(HttpError::Quic)?;
        let mut push_buf = Vec::new();
        encode_varint(PUSH_STREAM_TYPE, &mut push_buf).map_err(HttpError::Quic)?;
        encode_varint(push_id, &mut push_buf).map_err(HttpError::Quic)?;
        let response_block = self
            .encoder
            .encode_header_block(push_stream_id, response_headers)?;
        H3Frame::Headers(response_block).encode(&mut push_buf)?;
        if let Some(body) = body {
            if !body.is_empty() {
                H3Frame::Data(body.to_vec()).encode(&mut push_buf)?;
            }
        }
        self.quic
            .stream_send(push_stream_id, &push_buf, true)
            .map_err(HttpError::Quic)?;

        self.flush_qpack_stream_data().await?;
        self.flush_pending().await?;
        Ok(push_id)
    }

    /// Return the next completed pushed response, if one is ready.
    pub fn accept_push(&mut self) -> Option<H3Push> {
        self.ready_pushes.pop_front()
    }

    /// Cancel a pushed response by push ID.
    pub async fn cancel_push(&mut self, push_id: u64) -> Result<(), HttpError> {
        self.open_control_stream().await?;
        let control_stream_id = self
            .local_control_stream_id
            .ok_or_else(|| HttpError::Parse("local control stream missing".into()))?;

        let mut buf = Vec::new();
        H3Frame::CancelPush(push_id).encode(&mut buf)?;
        self.quic
            .stream_send(control_stream_id, &buf, false)
            .map_err(HttpError::Quic)?;
        self.flush_pending().await
    }

    /// Apply settings received from the peer.
    ///
    /// Stores the settings and updates encoder/decoder configuration
    /// (e.g. QPACK max table capacity) accordingly.
    /// Returns an error if duplicate setting identifiers are present.
    pub fn apply_peer_settings(&mut self, settings: Vec<H3Setting>) -> Result<(), HttpError> {
        validate_settings(&settings)?;
        let max_table_capacity = settings
            .iter()
            .find(|setting| setting.id == super::frame::setting_id::QPACK_MAX_TABLE_CAPACITY)
            .map(|setting| setting.value as usize)
            .unwrap_or(0);
        let blocked_streams = settings
            .iter()
            .find(|setting| setting.id == super::frame::setting_id::QPACK_BLOCKED_STREAMS)
            .map(|setting| setting.value as usize)
            .unwrap_or(0);
        self.encoder
            .apply_peer_settings(max_table_capacity, blocked_streams)?;
        self.peer_settings = Some(settings);
        Ok(())
    }

    /// Get the peer's settings, if received.
    pub fn peer_settings(&self) -> Option<&[H3Setting]> {
        self.peer_settings.as_deref()
    }

    /// Read and process frames from the peer's control stream.
    ///
    /// Reads incoming unidirectional streams looking for control and QPACK
    /// streams, then processes control frames incrementally.
    pub async fn poll_control(&mut self) -> Result<(), HttpError> {
        let mut recv_buf = vec![0u8; 4096];
        let mut pending_streams: HashSet<u64> = self.uni_stream_states.keys().copied().collect();
        // `accept_incoming_uni()` is a pure lookup, not a consuming iterator.
        // Snapshot the current ready streams once so we don't spin on the same ID.
        pending_streams.extend(self.quic.incoming_uni_stream_ids());

        for stream_id in pending_streams {
            let mut state = self
                .uni_stream_states
                .remove(&stream_id)
                .unwrap_or_default();
            let stream_finished = loop {
                let (n, fin) = match self.quic.stream_recv(stream_id, &mut recv_buf) {
                    Ok(result) => result,
                    Err(e) => return Err(HttpError::Quic(e)),
                };
                state.ingest(&recv_buf[..n], fin)?;
                self.process_uni_stream(stream_id, &mut state, fin)?;
                if fin || n == 0 {
                    break fin;
                }
            };

            if stream_finished && state.stream_type() == Some(PUSH_STREAM_TYPE) {
                self.finish_push_stream(stream_id, &mut state).await?;
            }

            if stream_finished && self.is_peer_critical_stream(stream_id) {
                return Err(HttpError::Parse(format!(
                    "H3_CLOSED_CRITICAL_STREAM (0x{:04x}): peer closed {} stream",
                    error::H3_CLOSED_CRITICAL_STREAM,
                    self.critical_stream_label(stream_id),
                )));
            }

            if !stream_finished {
                self.uni_stream_states.insert(stream_id, state);
            }
        }

        self.flush_qpack_stream_data().await?;
        Ok(())
    }

    fn process_uni_stream(
        &mut self,
        stream_id: u64,
        state: &mut UniStreamState,
        fin: bool,
    ) -> Result<(), HttpError> {
        match state.stream_type() {
            Some(CONTROL_STREAM_TYPE) => {
                Self::register_peer_uni_stream(
                    &mut self.peer_control_stream_id,
                    stream_id,
                    "control",
                )?;
                for frame in state.ingest_control_frames(fin)? {
                    self.process_control_frame(stream_id, frame)?;
                }
            }
            Some(QPACK_ENCODER_STREAM_TYPE) => {
                Self::register_peer_uni_stream(
                    &mut self.peer_qpack_encoder_stream_id,
                    stream_id,
                    "QPACK encoder",
                )?;
                if !state.pending.is_empty() {
                    self.decoder.feed_encoder_stream(&state.pending)?;
                    state.pending.clear();
                }
            }
            Some(QPACK_DECODER_STREAM_TYPE) => {
                Self::register_peer_uni_stream(
                    &mut self.peer_qpack_decoder_stream_id,
                    stream_id,
                    "QPACK decoder",
                )?;
                if !state.pending.is_empty() {
                    self.encoder.feed_decoder_stream(&state.pending)?;
                    state.pending.clear();
                }
            }
            Some(PUSH_STREAM_TYPE) => {
                if state.ingest_push_id(fin)?.is_some() {
                    state.ingest_push_frames(fin)?;
                }
            }
            Some(_) => {
                state.pending.clear();
            }
            None => {}
        }

        Ok(())
    }

    fn register_peer_uni_stream(
        slot: &mut Option<u64>,
        stream_id: u64,
        label: &str,
    ) -> Result<(), HttpError> {
        match slot {
            Some(existing) if *existing != stream_id => Err(HttpError::Parse(format!(
                "H3_STREAM_CREATION_ERROR (0x{:04x}): duplicate peer {label} stream",
                error::H3_STREAM_CREATION_ERROR,
            ))),
            Some(_) => Ok(()),
            None => {
                *slot = Some(stream_id);
                Ok(())
            }
        }
    }

    fn is_peer_critical_stream(&self, stream_id: u64) -> bool {
        self.peer_control_stream_id == Some(stream_id)
            || self.peer_qpack_encoder_stream_id == Some(stream_id)
            || self.peer_qpack_decoder_stream_id == Some(stream_id)
    }

    fn critical_stream_label(&self, stream_id: u64) -> &'static str {
        if self.peer_control_stream_id == Some(stream_id) {
            "control"
        } else if self.peer_qpack_encoder_stream_id == Some(stream_id) {
            "QPACK encoder"
        } else if self.peer_qpack_decoder_stream_id == Some(stream_id) {
            "QPACK decoder"
        } else {
            "critical"
        }
    }

    /// Process a single control frame, enforcing RFC 9114 rules.
    fn process_control_frame(&mut self, stream_id: u64, frame: H3Frame) -> Result<(), HttpError> {
        match frame {
            H3Frame::Settings(settings) => {
                if self.peer_control_stream_id != Some(stream_id) {
                    return Err(HttpError::Parse(format!(
                        "H3_STREAM_CREATION_ERROR (0x{:04x}): \
                         SETTINGS received on non-control stream",
                        error::H3_STREAM_CREATION_ERROR,
                    )));
                }
                self.apply_peer_settings(settings)?;
            }
            H3Frame::GoAway(last_stream_id) => {
                // RFC 9114 §5.2: GOAWAY indicates graceful shutdown.
                // The peer will not process requests on streams with
                // IDs greater than `last_stream_id`.
                // Subsequent GOAWAYs may only decrease the ID.
                if let Some(prev) = self.goaway_last_stream_id {
                    if last_stream_id > prev {
                        return Err(HttpError::Parse(format!(
                            "H3_ID_ERROR (0x{:04x}): GOAWAY stream ID increased \
                             from {} to {}",
                            error::H3_ID_ERROR,
                            prev,
                            last_stream_id,
                        )));
                    }
                }
                self.goaway_received = true;
                self.goaway_last_stream_id = Some(last_stream_id);
            }
            H3Frame::MaxPushId(max_push_id) => {
                reject_server_max_push_id(max_push_id)?;
            }
            H3Frame::CancelPush(_)
            | H3Frame::PriorityUpdateRequest { .. }
            | H3Frame::PriorityUpdatePush { .. } => {
                // Accepted on control stream, no action needed yet
            }
            _ => {
                // Unknown frame types are ignored per RFC 9114 §9
            }
        }
        Ok(())
    }

    /// Whether the peer has sent a GOAWAY frame.
    pub fn goaway_received(&self) -> bool {
        self.goaway_received
    }

    /// The last stream ID the peer will process, if GOAWAY was received.
    pub fn goaway_last_stream_id(&self) -> Option<u64> {
        self.goaway_last_stream_id
    }

    /// Get a mutable reference to the underlying QUIC connection.
    pub fn quic_mut(&mut self) -> &mut QuicConnection {
        &mut self.quic
    }

    async fn flush_qpack_stream_data(&mut self) -> Result<(), HttpError> {
        if !self.control_stream_opened {
            return Ok(());
        }

        if let Some(stream_id) = self.local_qpack_encoder_stream_id {
            let pending = self.encoder.take_pending_stream_data();
            if !pending.is_empty() {
                self.quic
                    .stream_send(stream_id, &pending, false)
                    .map_err(HttpError::Quic)?;
            }
        }

        if let Some(stream_id) = self.local_qpack_decoder_stream_id {
            let pending = self.decoder.take_pending_stream_data();
            if !pending.is_empty() {
                self.quic
                    .stream_send(stream_id, &pending, false)
                    .map_err(HttpError::Quic)?;
            }
        }

        Ok(())
    }

    async fn decode_header_block_wait(
        &mut self,
        stream_id: u64,
        block: &[u8],
    ) -> Result<Vec<(String, String)>, HttpError> {
        if let Some(headers) = self.decoder.decode_header_block(stream_id, block)? {
            self.flush_qpack_stream_data().await?;
            return Ok(headers);
        }

        let mut polls = 0;
        loop {
            polls += 1;
            if polls >= 1000 {
                return Err(HttpError::Qpack(
                    "header block remained blocked after repeated polls".into(),
                ));
            }

            Box::pin(self.poll()).await?;
            if let Some(headers) = self.decoder.try_unblocked(stream_id)? {
                self.flush_qpack_stream_data().await?;
                return Ok(headers);
            }
        }
    }

    async fn finish_push_stream(
        &mut self,
        stream_id: u64,
        state: &mut UniStreamState,
    ) -> Result<(), HttpError> {
        let push_id = state
            .ingest_push_id(true)?
            .ok_or_else(|| HttpError::Parse("missing push stream ID".into()))?;
        validate_incoming_push_id(self.local_max_push_id, push_id)?;
        ensure_initial_headers_seen(state.push_response_headers_block.is_some(), "push response")?;
        let response_headers = if let Some(block) = state.push_response_headers_block.take() {
            self.decode_header_block_wait(stream_id, &block).await?
        } else {
            Vec::new()
        };
        if let Some(block) = state.push_trailers_block.take() {
            let _trailers = self.decode_header_block_wait(stream_id, &block).await?;
        }

        let request_headers = self.promised_pushes.remove(&push_id).unwrap_or_default();
        self.ready_pushes.push_back(H3Push {
            push_id,
            request_headers,
            response_headers,
            body: std::mem::take(&mut state.push_body),
        });
        state.pending.clear();
        Ok(())
    }

    fn store_promised_push(
        &mut self,
        push_id: u64,
        request_headers: Vec<(String, String)>,
    ) -> Result<(), HttpError> {
        validate_incoming_push_id(self.local_max_push_id, push_id)?;
        if let Some(existing) = self
            .ready_pushes
            .iter_mut()
            .find(|push| push.push_id == push_id && push.request_headers.is_empty())
        {
            existing.request_headers = request_headers;
        } else {
            self.promised_pushes.insert(push_id, request_headers);
        }
        Ok(())
    }
}

/// Whether `next_push_id` is still within the peer's advertised limit.
///
/// RFC 9114 §7.2.7: `MAX_PUSH_ID` is the maximum push ID the server may use,
/// INCLUSIVE. The comparison must stay `<=` — using `<` blocks the final push
/// the peer explicitly permitted (e.g. MAX_PUSH_ID=16 allows 0..=16).
fn push_id_within_limit(next_push_id: u64, peer_max_push_id: u64) -> bool {
    next_push_id <= peer_max_push_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_stream_state_buffers_partial_settings() {
        let mut state = UniStreamState::default();
        let mut encoded = Vec::new();
        encode_varint(CONTROL_STREAM_TYPE, &mut encoded)
            .map_err(HttpError::Quic)
            .unwrap();
        H3Frame::Settings(vec![H3Setting { id: 0x01, value: 0 }])
            .encode(&mut encoded)
            .unwrap();

        state.ingest(&encoded[..2], false).unwrap();
        assert!(state.ingest_control_frames(false).unwrap().is_empty());

        state.ingest(&encoded[2..4], false).unwrap();
        assert!(state.ingest_control_frames(false).unwrap().is_empty());

        state.ingest(&encoded[4..], true).unwrap();
        let frames = state.ingest_control_frames(true).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(matches!(frames[0], H3Frame::Settings(_)));
    }

    #[test]
    fn control_stream_state_rejects_truncated_frame_at_fin() {
        let mut state = UniStreamState::default();
        let mut encoded = Vec::new();
        encode_varint(CONTROL_STREAM_TYPE, &mut encoded)
            .map_err(HttpError::Quic)
            .unwrap();
        H3Frame::Settings(vec![H3Setting { id: 0x01, value: 0 }])
            .encode(&mut encoded)
            .unwrap();

        state.ingest(&encoded[..encoded.len() - 1], true).unwrap();
        let err = state.ingest_control_frames(true).unwrap_err();
        assert!(matches!(err, HttpError::Parse(_)));
    }

    #[test]
    fn control_stream_rejects_missing_settings_first() {
        let mut state = UniStreamState::default();
        let mut encoded = Vec::new();
        encode_varint(CONTROL_STREAM_TYPE, &mut encoded).unwrap();
        // Send a GOAWAY before SETTINGS
        H3Frame::GoAway(0).encode(&mut encoded).unwrap();

        state.ingest(&encoded, true).unwrap();
        let err = state.ingest_control_frames(true).unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_MISSING_SETTINGS")),
            _ => panic!("expected Parse error, got {err:?}"),
        }
    }

    #[test]
    fn control_stream_rejects_data_frame() {
        let mut state = UniStreamState::default();
        let mut encoded = Vec::new();
        encode_varint(CONTROL_STREAM_TYPE, &mut encoded).unwrap();
        // SETTINGS first (valid)
        H3Frame::Settings(vec![]).encode(&mut encoded).unwrap();
        // Then a DATA frame (invalid on control stream)
        H3Frame::Data(b"hello".to_vec())
            .encode(&mut encoded)
            .unwrap();

        state.ingest(&encoded, true).unwrap();
        let err = state.ingest_control_frames(true).unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_FRAME_UNEXPECTED")),
            _ => panic!("expected Parse error, got {err:?}"),
        }
    }

    #[test]
    fn control_stream_rejects_headers_frame() {
        let mut state = UniStreamState::default();
        let mut encoded = Vec::new();
        encode_varint(CONTROL_STREAM_TYPE, &mut encoded).unwrap();
        H3Frame::Settings(vec![]).encode(&mut encoded).unwrap();
        H3Frame::Headers(vec![1, 2, 3])
            .encode(&mut encoded)
            .unwrap();

        state.ingest(&encoded, true).unwrap();
        let err = state.ingest_control_frames(true).unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_FRAME_UNEXPECTED")),
            _ => panic!("expected Parse error, got {err:?}"),
        }
    }

    #[test]
    fn control_stream_rejects_duplicate_settings() {
        let mut state = UniStreamState::default();
        let mut encoded = Vec::new();
        encode_varint(CONTROL_STREAM_TYPE, &mut encoded).unwrap();
        H3Frame::Settings(vec![]).encode(&mut encoded).unwrap();
        H3Frame::Settings(vec![]).encode(&mut encoded).unwrap();

        state.ingest(&encoded, true).unwrap();
        let err = state.ingest_control_frames(true).unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("duplicate SETTINGS")),
            _ => panic!("expected Parse error, got {err:?}"),
        }
    }

    #[test]
    fn ensure_initial_headers_seen_rejects_missing_initial_headers() {
        let err = ensure_initial_headers_seen(false, "response").unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_MESSAGE_ERROR")),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn validate_incoming_push_id_rejects_exceeding_local_limit() {
        let err = validate_incoming_push_id(Some(16), 17).unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_ID_ERROR")),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn validate_incoming_push_id_rejects_unadvertised_pushes() {
        let err = validate_incoming_push_id(None, 0).unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_ID_ERROR")),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn validate_local_max_push_id_rejects_decrease() {
        let current = Some(8);
        let err = validate_local_max_push_id(current, 7).unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_ID_ERROR")),
            other => panic!("expected Parse error, got {other:?}"),
        }
        assert_eq!(current, Some(8));
    }

    #[test]
    fn non_control_uni_streams_clear_payload_after_type() {
        let mut state = UniStreamState::default();
        let mut encoded = Vec::new();
        encode_varint(QPACK_DECODER_STREAM_TYPE, &mut encoded).unwrap();
        encoded.extend_from_slice(b"ignored");

        state.ingest(&encoded, false).unwrap();
        assert_eq!(state.stream_type(), Some(QPACK_DECODER_STREAM_TYPE));
    }

    #[test]
    fn control_stream_state_rejects_excessive_buffering() {
        let mut state = UniStreamState::default();
        let mut encoded = Vec::new();
        encode_varint(CONTROL_STREAM_TYPE, &mut encoded).unwrap();
        H3Frame::Unknown {
            frame_type: 0x21,
            data: vec![0u8; MAX_CONTROL_STREAM_PENDING_BYTES + 1],
        }
        .encode(&mut encoded)
        .unwrap();

        let err = state.ingest(&encoded, false).unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_EXCESSIVE_LOAD")),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn push_stream_state_parses_frames_incrementally() {
        let mut state = UniStreamState::default();
        let mut encoded = Vec::new();
        encode_varint(PUSH_STREAM_TYPE, &mut encoded).unwrap();
        encode_varint(7, &mut encoded).unwrap();
        H3Frame::Headers(vec![1, 2, 3])
            .encode(&mut encoded)
            .unwrap();
        H3Frame::Data(b"hello".to_vec())
            .encode(&mut encoded)
            .unwrap();

        state.ingest(&encoded, false).unwrap();
        assert_eq!(state.ingest_push_id(false).unwrap(), Some(7));
        state.ingest_push_frames(false).unwrap();

        assert!(state.pending.is_empty());
        assert_eq!(state.push_response_headers_block, Some(vec![1, 2, 3]));
        assert_eq!(state.push_body, b"hello");
    }

    #[test]
    fn push_stream_state_rejects_excessive_body_buffering() {
        let mut state = UniStreamState::default();
        let mut setup = Vec::new();
        encode_varint(PUSH_STREAM_TYPE, &mut setup).unwrap();
        encode_varint(7, &mut setup).unwrap();
        H3Frame::Headers(vec![1, 2, 3]).encode(&mut setup).unwrap();

        state.ingest(&setup, false).unwrap();
        assert_eq!(state.ingest_push_id(false).unwrap(), Some(7));
        state.ingest_push_frames(false).unwrap();

        let chunk_len = 8 * 1024;
        let mut chunk = Vec::new();
        H3Frame::Data(vec![0u8; chunk_len])
            .encode(&mut chunk)
            .unwrap();

        for _ in 0..(MAX_PUSH_STREAM_BODY_BYTES / chunk_len) {
            state.ingest(&chunk, false).unwrap();
            state.ingest_push_frames(false).unwrap();
        }

        let mut overflow = Vec::new();
        H3Frame::Data(vec![0u8; 1]).encode(&mut overflow).unwrap();
        state.ingest(&overflow, false).unwrap();
        let err = state.ingest_push_frames(false).unwrap_err();

        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_EXCESSIVE_LOAD")),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn validate_settings_rejects_duplicate_ids() {
        let result = validate_settings(&[
            H3Setting {
                id: 0x01,
                value: 100,
            },
            H3Setting {
                id: 0x01,
                value: 200,
            },
        ]);
        assert!(result.is_err());
        match result.unwrap_err() {
            HttpError::Parse(msg) => assert!(msg.contains("H3_SETTINGS_ERROR")),
            e => panic!("expected Parse error, got {e:?}"),
        }
    }

    #[test]
    fn validate_settings_rejects_reserved_ids() {
        let result = validate_settings(&[H3Setting { id: 0x02, value: 1 }]);
        assert!(result.is_err());
        match result.unwrap_err() {
            HttpError::Parse(msg) => assert!(msg.contains("H3_SETTINGS_ERROR")),
            e => panic!("expected Parse error, got {e:?}"),
        }
    }

    #[test]
    fn reject_server_max_push_id_frame_is_unexpected() {
        let err = reject_server_max_push_id(4).unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_FRAME_UNEXPECTED")),
            e => panic!("expected Parse error, got {e:?}"),
        }
    }

    #[test]
    fn validate_settings_accepts_unique_ids() {
        let result = validate_settings(&[
            H3Setting {
                id: 0x01,
                value: 100,
            },
            H3Setting {
                id: 0x06,
                value: 200,
            },
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn push_id_limit_is_inclusive() {
        assert!(push_id_within_limit(0, 0));
        assert!(push_id_within_limit(16, 16));
        assert!(!push_id_within_limit(17, 16));
    }
}
