//! HTTP/3 server (RFC 9114).
//!
//! [`H3Server`] listens for incoming HTTP/3 connections and processes
//! requests over QUIC streams via the endpoint-based server transport.

use std::collections::{HashMap, HashSet};
use std::mem;
use std::net::SocketAddr;

use starfish_quic::{
    decode_varint, encode_varint, ConnectionHandle, QuicEndpoint, QuicServerConfig,
};
use starfish_reactor::cooperative_synchronization::lock::CooperativeFairLock;

use super::error;
use super::force_h3_alpn;
use super::frame::{decode_all_frames_at_eof, is_reserved_http2_setting_id, H3Frame, H3Setting};
use super::qpack::decoder::QpackDecoder;
use super::qpack::encoder::QpackEncoder;
use crate::error::HttpError;

const CONTROL_STREAM_TYPE: u64 = 0x00;
const QPACK_ENCODER_STREAM_TYPE: u64 = 0x02;
const QPACK_DECODER_STREAM_TYPE: u64 = 0x03;
const LOCAL_QPACK_MAX_TABLE_CAPACITY: u64 = 0;
const LOCAL_QPACK_BLOCKED_STREAMS: u64 = 0;
const MAX_UNI_STREAM_TYPE_BYTES: usize = 8;
const MAX_UNI_STREAM_PENDING_BYTES: usize = 64 * 1024;

/// HTTP/3 server listener.
pub struct H3Server {
    endpoint: CooperativeFairLock<QuicEndpoint>,
}

impl H3Server {
    /// Bind an HTTP/3 server to the given address.
    pub async fn bind(addr: SocketAddr, mut config: QuicServerConfig) -> Result<Self, HttpError> {
        force_h3_alpn(&mut config.tls_config.alpn_protocols);
        let endpoint = QuicEndpoint::bind(addr, config)
            .await
            .map_err(HttpError::Quic)?;
        Ok(Self {
            endpoint: CooperativeFairLock::new(endpoint),
        })
    }

    /// Accept the next HTTP/3 connection.
    pub async fn accept(&mut self) -> Result<H3ServerConnection, HttpError> {
        let handle = {
            let mut endpoint = self.endpoint.acquire().await;
            endpoint.accept().await.map_err(HttpError::Quic)?
        };

        let mut conn = H3ServerConnection::new(self.endpoint.clone(), handle);
        conn.open_control_stream().await?;
        Ok(conn)
    }
}

/// A single HTTP/3 server connection, handling requests from one client.
pub struct H3ServerConnection {
    endpoint: CooperativeFairLock<QuicEndpoint>,
    handle: ConnectionHandle,
    encoder: QpackEncoder,
    decoder: QpackDecoder,
    peer_settings: Option<Vec<H3Setting>>,
    control_stream_opened: bool,
    local_control_stream_id: Option<u64>,
    local_qpack_encoder_stream_id: Option<u64>,
    local_qpack_decoder_stream_id: Option<u64>,
    uni_stream_states: HashMap<u64, UniStreamState>,
    peer_control_stream_id: Option<u64>,
    peer_qpack_encoder_stream_id: Option<u64>,
    peer_qpack_decoder_stream_id: Option<u64>,
    peer_max_push_id: u64,
    next_local_push_id: u64,
}

impl H3ServerConnection {
    fn new(endpoint: CooperativeFairLock<QuicEndpoint>, handle: ConnectionHandle) -> Self {
        Self {
            endpoint,
            handle,
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
            peer_max_push_id: 0,
            next_local_push_id: 0,
        }
    }

    async fn open_control_stream(&mut self) -> Result<(), HttpError> {
        if self.control_stream_opened {
            return Ok(());
        }

        let mut endpoint = self.endpoint.acquire().await;
        let control_stream_id = endpoint
            .open_uni_stream(self.handle)
            .map_err(HttpError::Quic)?;
        let encoder_stream_id = endpoint
            .open_uni_stream(self.handle)
            .map_err(HttpError::Quic)?;
        let decoder_stream_id = endpoint
            .open_uni_stream(self.handle)
            .map_err(HttpError::Quic)?;

        let mut control_buf = Vec::new();
        encode_varint(CONTROL_STREAM_TYPE, &mut control_buf).map_err(HttpError::Quic)?;
        H3Frame::Settings(vec![
            H3Setting {
                id: super::frame::setting_id::QPACK_MAX_TABLE_CAPACITY,
                value: LOCAL_QPACK_MAX_TABLE_CAPACITY,
            },
            H3Setting {
                id: super::frame::setting_id::QPACK_BLOCKED_STREAMS,
                value: LOCAL_QPACK_BLOCKED_STREAMS,
            },
        ])
        .encode(&mut control_buf)?;
        endpoint
            .stream_send(self.handle, control_stream_id, &control_buf, false)
            .map_err(HttpError::Quic)?;

        let mut encoder_buf = Vec::new();
        encode_varint(QPACK_ENCODER_STREAM_TYPE, &mut encoder_buf).map_err(HttpError::Quic)?;
        endpoint
            .stream_send(self.handle, encoder_stream_id, &encoder_buf, false)
            .map_err(HttpError::Quic)?;

        let mut decoder_buf = Vec::new();
        encode_varint(QPACK_DECODER_STREAM_TYPE, &mut decoder_buf).map_err(HttpError::Quic)?;
        endpoint
            .stream_send(self.handle, decoder_stream_id, &decoder_buf, false)
            .map_err(HttpError::Quic)?;

        endpoint.flush_all().await;

        self.control_stream_opened = true;
        self.local_control_stream_id = Some(control_stream_id);
        self.local_qpack_encoder_stream_id = Some(encoder_stream_id);
        self.local_qpack_decoder_stream_id = Some(decoder_stream_id);
        Ok(())
    }

    /// Accept the next request on this connection.
    ///
    /// Returns the stream ID and parsed request, or `None` if the connection is closing.
    pub async fn recv_request(
        &mut self,
    ) -> Result<Option<(u64, http::Request<Vec<u8>>)>, HttpError> {
        let stream_id = loop {
            self.poll_control().await?;
            if let Some(id) = self.accept_incoming_bidi().await? {
                break id;
            }

            self.poll().await?;
            if let Some(id) = self.accept_incoming_bidi().await? {
                break id;
            }
        };

        let (headers, body) = self.read_request(stream_id).await?;
        Ok(Some((stream_id, request_from_h3_parts(headers, body)?)))
    }

    /// Send a response on a request stream.
    pub async fn send_response(
        &mut self,
        stream_id: u64,
        response: http::Response<Vec<u8>>,
    ) -> Result<(), HttpError> {
        self.open_control_stream().await?;
        self.poll_control().await?;

        let status = response.status().as_u16().to_string();
        let mut headers: Vec<(String, String)> = vec![(":status".to_string(), status)];
        for (name, value) in response.headers() {
            if let Ok(v) = value.to_str() {
                headers.push((name.as_str().to_string(), v.to_string()));
            }
        }

        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();

        let mut buf = Vec::new();
        let header_block = self.encoder.encode_header_block(stream_id, &header_refs)?;
        H3Frame::Headers(header_block).encode(&mut buf)?;
        if !response.body().is_empty() {
            H3Frame::Data(response.body().clone()).encode(&mut buf)?;
        }

        self.flush_qpack_stream_data().await?;
        {
            let mut endpoint = self.endpoint.acquire().await;
            endpoint
                .stream_send(self.handle, stream_id, &buf, true)
                .map_err(HttpError::Quic)?;
            endpoint.flush_all().await;
        }
        self.poll_control().await
    }

    /// Send a server push promise plus pushed response for a parent request stream.
    pub async fn send_push(
        &mut self,
        parent_stream_id: u64,
        request: http::Request<Vec<u8>>,
        response: http::Response<Vec<u8>>,
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

        let request_headers = request_into_h3_headers(&request);
        let request_refs: Vec<(&str, &str)> = request_headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();

        let mut promise_buf = Vec::new();
        let request_block = self
            .encoder
            .encode_header_block(parent_stream_id, &request_refs)?;
        H3Frame::PushPromise {
            push_id,
            headers: request_block,
        }
        .encode(&mut promise_buf)?;

        let status = response.status().as_u16().to_string();
        let mut response_headers: Vec<(String, String)> = vec![(":status".to_string(), status)];
        for (name, value) in response.headers() {
            if let Ok(v) = value.to_str() {
                response_headers.push((name.as_str().to_string(), v.to_string()));
            }
        }
        let response_refs: Vec<(&str, &str)> = response_headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();

        let mut endpoint = self.endpoint.acquire().await;
        endpoint
            .stream_send(self.handle, parent_stream_id, &promise_buf, false)
            .map_err(HttpError::Quic)?;

        let push_stream_id = endpoint
            .open_uni_stream(self.handle)
            .map_err(HttpError::Quic)?;
        let mut push_buf = Vec::new();
        encode_varint(0x01, &mut push_buf).map_err(HttpError::Quic)?;
        encode_varint(push_id, &mut push_buf).map_err(HttpError::Quic)?;
        let response_block = self
            .encoder
            .encode_header_block(push_stream_id, &response_refs)?;
        H3Frame::Headers(response_block).encode(&mut push_buf)?;
        if !response.body().is_empty() {
            H3Frame::Data(response.body().clone()).encode(&mut push_buf)?;
        }

        drop(endpoint);
        self.flush_qpack_stream_data().await?;

        let mut endpoint = self.endpoint.acquire().await;
        endpoint
            .stream_send(self.handle, push_stream_id, &push_buf, true)
            .map_err(HttpError::Quic)?;
        endpoint.flush_all().await;
        drop(endpoint);

        self.poll_control().await?;
        Ok(push_id)
    }

    /// Drive the underlying QUIC/H3 connection.
    pub async fn poll(&mut self) -> Result<(), HttpError> {
        {
            let mut endpoint = self.endpoint.acquire().await;
            endpoint.drive().await.map_err(HttpError::Quic)?;
        }
        self.poll_control().await
    }

    async fn accept_incoming_bidi(&self) -> Result<Option<u64>, HttpError> {
        let endpoint = self.endpoint.acquire().await;
        Ok(endpoint.accept_incoming_bidi(self.handle))
    }

    async fn read_request(
        &mut self,
        stream_id: u64,
    ) -> Result<(Vec<(String, String)>, Vec<u8>), HttpError> {
        self.poll_control().await?;

        let mut recv_buf = vec![0u8; 16384];
        let mut accumulated = Vec::new();
        let mut empty_polls = 0;

        loop {
            let (n, fin) = {
                let mut endpoint = self.endpoint.acquire().await;
                endpoint
                    .stream_recv(self.handle, stream_id, &mut recv_buf)
                    .map_err(HttpError::Quic)?
            };
            if n > 0 {
                accumulated.extend_from_slice(&recv_buf[..n]);
                empty_polls = 0;
            }
            if fin {
                break;
            }
            if n == 0 {
                empty_polls += 1;
                if empty_polls >= 1000 {
                    return Err(HttpError::Parse(
                        "read_request: no progress after repeated polls".into(),
                    ));
                }
                self.poll().await?;
            }
        }

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
                H3Frame::PushPromise { .. } => {
                    return Err(h3_frame_unexpected("PUSH_PROMISE on request stream"));
                }
                H3Frame::Unknown { .. } => {}
            }
        }

        ensure_initial_headers_seen(saw_initial_headers, "request")?;
        Ok((headers, body))
    }

    async fn poll_control(&mut self) -> Result<(), HttpError> {
        let mut recv_buf = vec![0u8; 4096];
        let mut pending_streams: HashSet<u64> = self.uni_stream_states.keys().copied().collect();
        {
            let endpoint = self.endpoint.acquire().await;
            pending_streams.extend(endpoint.incoming_uni_stream_ids(self.handle));
        }

        for stream_id in pending_streams {
            let mut state = self
                .uni_stream_states
                .remove(&stream_id)
                .unwrap_or_default();
            let stream_finished = loop {
                let (n, fin) = {
                    let mut endpoint = self.endpoint.acquire().await;
                    endpoint
                        .stream_recv(self.handle, stream_id, &mut recv_buf)
                        .map_err(HttpError::Quic)?
                };
                state.ingest(&recv_buf[..n], fin)?;
                self.process_uni_stream(stream_id, &mut state, fin)?;
                if fin || n == 0 {
                    break fin;
                }
            };

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
                register_peer_uni_stream(&mut self.peer_control_stream_id, stream_id, "control")?;
                for frame in state.ingest_control_frames(fin)? {
                    self.process_control_frame(stream_id, frame)?;
                }
            }
            Some(QPACK_ENCODER_STREAM_TYPE) => {
                register_peer_uni_stream(
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
                register_peer_uni_stream(
                    &mut self.peer_qpack_decoder_stream_id,
                    stream_id,
                    "QPACK decoder",
                )?;
                if !state.pending.is_empty() {
                    self.encoder.feed_decoder_stream(&state.pending)?;
                    state.pending.clear();
                }
            }
            Some(_) => {
                state.pending.clear();
            }
            None => {}
        }

        Ok(())
    }

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
            H3Frame::GoAway(_) => {}
            H3Frame::MaxPushId(max_push_id) => {
                update_peer_max_push_id(&mut self.peer_max_push_id, max_push_id)?;
            }
            H3Frame::CancelPush(_)
            | H3Frame::PriorityUpdateRequest { .. }
            | H3Frame::PriorityUpdatePush { .. } => {}
            H3Frame::Data(_)
            | H3Frame::Headers(_)
            | H3Frame::PushPromise { .. }
            | H3Frame::Unknown { .. } => {}
        }
        Ok(())
    }

    fn apply_peer_settings(&mut self, settings: Vec<H3Setting>) -> Result<(), HttpError> {
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

    async fn flush_qpack_stream_data(&mut self) -> Result<(), HttpError> {
        if !self.control_stream_opened {
            return Ok(());
        }

        let encoder_pending = self.encoder.take_pending_stream_data();
        let decoder_pending = self.decoder.take_pending_stream_data();

        if encoder_pending.is_empty() && decoder_pending.is_empty() {
            return Ok(());
        }

        let mut endpoint = self.endpoint.acquire().await;
        if let Some(stream_id) = self.local_qpack_encoder_stream_id {
            if !encoder_pending.is_empty() {
                endpoint
                    .stream_send(self.handle, stream_id, &encoder_pending, false)
                    .map_err(HttpError::Quic)?;
            }
        }
        if let Some(stream_id) = self.local_qpack_decoder_stream_id {
            if !decoder_pending.is_empty() {
                endpoint
                    .stream_send(self.handle, stream_id, &decoder_pending, false)
                    .map_err(HttpError::Quic)?;
            }
        }
        endpoint.flush_all().await;
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

            self.poll().await?;
            if let Some(headers) = self.decoder.try_unblocked(stream_id)? {
                self.flush_qpack_stream_data().await?;
                return Ok(headers);
            }
        }
    }
}

#[derive(Debug, Default)]
struct UniStreamState {
    stream_type: Option<u64>,
    pending: Vec<u8>,
    settings_seen: bool,
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

    fn ingest_control_frames(&mut self, fin: bool) -> Result<Vec<H3Frame>, HttpError> {
        let mut frames = Vec::new();
        loop {
            match H3Frame::decode(&self.pending)? {
                Some((frame, consumed)) => {
                    self.pending.drain(..consumed);
                    if !self.settings_seen {
                        match &frame {
                            H3Frame::Settings(_) => self.settings_seen = true,
                            _ => {
                                return Err(HttpError::Parse(format!(
                                    "H3_MISSING_SETTINGS (0x{:04x}): first frame on control stream must be SETTINGS, got {:?}",
                                    error::H3_MISSING_SETTINGS,
                                    mem::discriminant(&frame),
                                )));
                            }
                        }
                    } else if matches!(frame, H3Frame::Settings(_)) {
                        return Err(h3_frame_unexpected(
                            "duplicate SETTINGS frame on control stream",
                        ));
                    }

                    match &frame {
                        H3Frame::Data(_) | H3Frame::Headers(_) | H3Frame::PushPromise { .. } => {
                            return Err(HttpError::Parse(format!(
                                "H3_FRAME_UNEXPECTED (0x{:04x}): frame type not allowed on control stream",
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

    fn ensure_pending_limit(&self) -> Result<(), HttpError> {
        if self.stream_type.is_some() && self.pending.len() > MAX_UNI_STREAM_PENDING_BYTES {
            return Err(HttpError::Parse(format!(
                "H3_EXCESSIVE_LOAD (0x{:04x}): peer unidirectional stream buffered \
                 more than {} bytes without a complete frame",
                error::H3_EXCESSIVE_LOAD,
                MAX_UNI_STREAM_PENDING_BYTES,
            )));
        }

        Ok(())
    }
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

fn h3_message_error(message: impl Into<String>) -> HttpError {
    HttpError::Parse(format!(
        "H3_MESSAGE_ERROR (0x{:04x}): {}",
        error::H3_MESSAGE_ERROR,
        message.into(),
    ))
}

fn h3_frame_unexpected(message: impl Into<String>) -> HttpError {
    HttpError::Parse(format!(
        "H3_FRAME_UNEXPECTED (0x{:04x}): {}",
        error::H3_FRAME_UNEXPECTED,
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

fn h3_settings_error(message: impl Into<String>) -> HttpError {
    HttpError::Parse(format!(
        "H3_SETTINGS_ERROR (0x{:04x}): {}",
        error::H3_SETTINGS_ERROR,
        message.into(),
    ))
}

fn update_peer_max_push_id(current: &mut u64, max_push_id: u64) -> Result<(), HttpError> {
    if max_push_id < *current {
        return Err(h3_id_error(format!(
            "peer reduced MAX_PUSH_ID from {} to {}",
            *current, max_push_id,
        )));
    }

    *current = max_push_id;
    Ok(())
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

#[derive(Default)]
struct RequestPseudoHeaders<'a> {
    method: Option<&'a str>,
    scheme: Option<&'a str>,
    authority: Option<&'a str>,
    path: Option<&'a str>,
}

fn set_request_pseudo_header<'a>(
    slot: &mut Option<&'a str>,
    name: &str,
    value: &'a str,
) -> Result<(), HttpError> {
    if slot.replace(value).is_some() {
        return Err(h3_message_error(format!("duplicate {name} pseudo-header",)));
    }

    Ok(())
}

fn validate_request_pseudo_headers(
    headers: &[(String, String)],
) -> Result<RequestPseudoHeaders<'_>, HttpError> {
    let mut pseudo = RequestPseudoHeaders::default();
    let mut saw_regular_header = false;

    for (name, value) in headers {
        if name.starts_with(':') {
            if saw_regular_header {
                return Err(h3_message_error(
                    "request pseudo-header field appeared after a regular header field",
                ));
            }

            match name.as_str() {
                ":method" => set_request_pseudo_header(&mut pseudo.method, name, value)?,
                ":scheme" => set_request_pseudo_header(&mut pseudo.scheme, name, value)?,
                ":authority" => set_request_pseudo_header(&mut pseudo.authority, name, value)?,
                ":path" => set_request_pseudo_header(&mut pseudo.path, name, value)?,
                _ => {
                    return Err(h3_message_error(format!(
                        "unexpected request pseudo-header {name}",
                    )));
                }
            }
        } else {
            saw_regular_header = true;
        }
    }

    let method = pseudo
        .method
        .ok_or_else(|| h3_message_error("missing :method pseudo-header"))?;
    if method == http::Method::CONNECT.as_str() {
        if pseudo.scheme.is_some() || pseudo.path.is_some() {
            return Err(h3_message_error(
                "CONNECT request must not include :scheme or :path pseudo-headers",
            ));
        }
        if pseudo.authority.is_none() {
            return Err(h3_message_error(
                "CONNECT request is missing :authority pseudo-header",
            ));
        }
    } else {
        if pseudo.scheme.is_none() {
            return Err(h3_message_error("missing :scheme pseudo-header"));
        }
        if pseudo.path.is_none() {
            return Err(h3_message_error("missing :path pseudo-header"));
        }
    }

    Ok(pseudo)
}

fn validate_settings(settings: &[H3Setting]) -> Result<(), HttpError> {
    let mut seen_ids = HashSet::new();
    for setting in settings {
        if is_reserved_http2_setting_id(setting.id) {
            return Err(h3_settings_error(format!(
                "reserved HTTP/2 setting id 0x{:02x}",
                setting.id,
            )));
        }
        if !seen_ids.insert(setting.id) {
            return Err(h3_settings_error(format!(
                "duplicate setting id 0x{:02x}",
                setting.id,
            )));
        }
    }
    Ok(())
}

/// Whether `next_push_id` is still within the peer's advertised limit.
///
/// RFC 9114 §7.2.7: `MAX_PUSH_ID` is the maximum push ID the server may use,
/// INCLUSIVE. The comparison must stay `<=` — using `<` blocks the final push
/// the peer explicitly permitted (e.g. MAX_PUSH_ID=16 allows 0..=16).
fn push_id_within_limit(next_push_id: u64, peer_max_push_id: u64) -> bool {
    next_push_id <= peer_max_push_id
}

fn request_from_h3_parts(
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<http::Request<Vec<u8>>, HttpError> {
    let pseudo = validate_request_pseudo_headers(&headers)?;
    let method: http::Method = pseudo
        .method
        .expect("validated :method pseudo-header")
        .parse()
        .map_err(|e| HttpError::Parse(format!("invalid method: {e}")))?;

    let uri = if method == http::Method::CONNECT {
        pseudo
            .authority
            .expect("validated CONNECT authority")
            .parse()
            .map_err(|e| HttpError::Parse(format!("invalid URI: {e}")))?
    } else if let Some(authority) = pseudo.authority {
        http::Uri::builder()
            .scheme(pseudo.scheme.expect("validated :scheme pseudo-header"))
            .authority(authority)
            .path_and_query(pseudo.path.expect("validated :path pseudo-header"))
            .build()
            .map_err(|e| HttpError::Parse(format!("invalid URI: {e}")))?
    } else {
        pseudo
            .path
            .expect("validated :path pseudo-header")
            .parse()
            .map_err(|e| HttpError::Parse(format!("invalid URI: {e}")))?
    };

    let mut builder = http::Request::builder().method(method).uri(uri);
    for (name, value) in &headers {
        if !name.starts_with(':') {
            builder = builder.header(name.as_str(), value.as_str());
        }
    }

    builder
        .body(body)
        .map_err(|e| HttpError::Parse(format!("build request: {e}")))
}

fn request_into_h3_headers(request: &http::Request<Vec<u8>>) -> Vec<(String, String)> {
    let method = request.method().as_str().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let scheme = request.uri().scheme_str().unwrap_or("https").to_string();
    let authority = request
        .uri()
        .authority()
        .map(|value| value.as_str().to_string())
        .unwrap_or_default();

    let mut headers = vec![
        (":method".to_string(), method),
        (":path".to_string(), path),
        (":scheme".to_string(), scheme),
    ];
    if !authority.is_empty() {
        headers.push((":authority".to_string(), authority));
    }
    for (name, value) in request.headers() {
        if let Ok(value) = value.to_str() {
            headers.push((name.as_str().to_string(), value.to_string()));
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_initial_headers_seen, force_h3_alpn, push_id_within_limit, request_from_h3_parts,
        update_peer_max_push_id, validate_settings, UniStreamState, CONTROL_STREAM_TYPE,
        MAX_UNI_STREAM_PENDING_BYTES,
    };
    use crate::error::HttpError;
    use starfish_quic::encode_varint;

    use crate::h3::frame::{H3Frame, H3Setting};

    #[test]
    fn force_h3_alpn_overrides_existing_protocols() {
        let mut protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        force_h3_alpn(&mut protocols);
        assert_eq!(protocols, vec![b"h3".to_vec()]);
    }

    #[test]
    fn request_from_h3_parts_rejects_invalid_method() {
        let err = request_from_h3_parts(
            vec![
                (":method".into(), "BAD METHOD".into()),
                (":scheme".into(), "https".into()),
                (":path".into(), "/".into()),
            ],
            Vec::new(),
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("invalid method")));
    }

    #[test]
    fn request_from_h3_parts_rejects_invalid_uri() {
        let err = request_from_h3_parts(
            vec![
                (":method".into(), "GET".into()),
                (":scheme".into(), "https".into()),
                (":authority".into(), "example.com".into()),
                (":path".into(), "bad path".into()),
            ],
            Vec::new(),
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("invalid URI")));
    }

    #[test]
    fn request_from_h3_parts_rejects_invalid_header_name() {
        let err = request_from_h3_parts(
            vec![
                (":method".into(), "GET".into()),
                (":scheme".into(), "https".into()),
                (":path".into(), "/".into()),
                ("bad header".into(), "value".into()),
            ],
            Vec::new(),
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("build request")));
    }

    #[test]
    fn request_from_h3_parts_rejects_missing_method() {
        let err = request_from_h3_parts(
            vec![
                (":scheme".into(), "https".into()),
                (":path".into(), "/".into()),
            ],
            Vec::new(),
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_MESSAGE_ERROR")));
    }

    #[test]
    fn request_from_h3_parts_rejects_missing_scheme() {
        let err = request_from_h3_parts(
            vec![
                (":method".into(), "GET".into()),
                (":path".into(), "/".into()),
            ],
            Vec::new(),
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_MESSAGE_ERROR")));
    }

    #[test]
    fn request_from_h3_parts_rejects_duplicate_method() {
        let err = request_from_h3_parts(
            vec![
                (":method".into(), "GET".into()),
                (":method".into(), "POST".into()),
                (":scheme".into(), "https".into()),
                (":path".into(), "/".into()),
            ],
            Vec::new(),
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_MESSAGE_ERROR")));
    }

    #[test]
    fn request_from_h3_parts_rejects_unexpected_pseudo_header() {
        let err = request_from_h3_parts(
            vec![
                (":method".into(), "GET".into()),
                (":scheme".into(), "https".into()),
                (":path".into(), "/".into()),
                (":status".into(), "200".into()),
            ],
            Vec::new(),
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_MESSAGE_ERROR")));
    }

    #[test]
    fn request_from_h3_parts_rejects_pseudo_header_after_regular_header() {
        let err = request_from_h3_parts(
            vec![
                (":method".into(), "GET".into()),
                ("x-test".into(), "1".into()),
                (":scheme".into(), "https".into()),
                (":path".into(), "/".into()),
            ],
            Vec::new(),
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_MESSAGE_ERROR")));
    }

    #[test]
    fn request_from_h3_parts_rejects_connect_without_authority() {
        let err = request_from_h3_parts(vec![(":method".into(), "CONNECT".into())], Vec::new())
            .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_MESSAGE_ERROR")));
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
    fn update_peer_max_push_id_rejects_decrease() {
        let mut current = 8;
        let err = update_peer_max_push_id(&mut current, 7).unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_ID_ERROR")),
            e => panic!("expected Parse error, got {e:?}"),
        }
    }

    #[test]
    fn push_id_limit_is_inclusive() {
        assert!(push_id_within_limit(0, 0));
        assert!(push_id_within_limit(16, 16));
        assert!(!push_id_within_limit(17, 16));
    }

    #[test]
    fn control_stream_state_rejects_excessive_buffering() {
        let mut state = UniStreamState::default();
        let mut encoded = Vec::new();
        encode_varint(CONTROL_STREAM_TYPE, &mut encoded).unwrap();
        H3Frame::Unknown {
            frame_type: 0x21,
            data: vec![0u8; MAX_UNI_STREAM_PENDING_BYTES + 1],
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
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn ensure_initial_headers_seen_rejects_missing_initial_headers() {
        let err = ensure_initial_headers_seen(false, "request").unwrap_err();
        match err {
            HttpError::Parse(msg) => assert!(msg.contains("H3_MESSAGE_ERROR")),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }
}
