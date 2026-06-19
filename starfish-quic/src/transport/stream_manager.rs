//! Stream lifecycle management (RFC 9000 §3).
//!
//! QUIC streams have independent send and receive state machines.
//! Stream IDs encode directionality and initiator:
//! - Bit 0: 0 = client-initiated, 1 = server-initiated
//! - Bit 1: 0 = bidirectional, 1 = unidirectional

use crate::error::{QuicError, TransportErrorCode};
use crate::transport::flow_control::StreamFlowControl;
use std::collections::HashMap;

/// Stream ID type.
pub type StreamId = u64;

/// Who initiated the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamInitiator {
    Client,
    Server,
}

/// Stream directionality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDirection {
    Bidirectional,
    Unidirectional,
}

/// Send-side state machine (RFC 9000 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendState {
    Ready,
    Send,
    DataSent,
    ResetSent,
    DataRecvd,
    ResetRecvd,
}

/// Receive-side state machine (RFC 9000 §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvState {
    Recv,
    SizeKnown,
    DataRecvd,
    ResetRecvd,
    DataRead,
    ResetRead,
}

/// Per-stream state.
#[derive(Debug)]
pub struct StreamState {
    pub id: StreamId,
    pub send: Option<SendState>,
    pub recv: Option<RecvState>,
    pub flow_control: StreamFlowControl,
    /// Buffered received data, keyed by offset.
    pub recv_buf: HashMap<u64, Vec<u8>>,
    /// Next expected receive offset (for in-order delivery).
    pub recv_offset: u64,
    /// The final size, if known (FIN received).
    pub final_size: Option<u64>,
    /// Application error code received in RESET_STREAM, if any.
    pub reset_error_code: Option<u64>,
    /// Next send offset.
    pub send_offset: u64,
    /// Whether we have sent FIN.
    pub fin_sent: bool,
    /// Acknowledged sent byte ranges, stored as sorted half-open intervals.
    pub acked_send_ranges: Vec<(u64, u64)>,
    /// Whether a FIN-bearing STREAM frame has been acknowledged.
    pub fin_acked: bool,
}

impl StreamState {
    fn new(
        id: StreamId,
        direction: StreamDirection,
        is_local: bool,
        initial_send_max: u64,
        initial_recv_max: u64,
    ) -> Self {
        let (send, recv) = match direction {
            StreamDirection::Bidirectional => (Some(SendState::Ready), Some(RecvState::Recv)),
            StreamDirection::Unidirectional => {
                if is_local {
                    (Some(SendState::Ready), None) // local uni = send only
                } else {
                    (None, Some(RecvState::Recv)) // remote uni = recv only
                }
            }
        };
        Self {
            id,
            send,
            recv,
            flow_control: StreamFlowControl::new(initial_send_max, initial_recv_max),
            recv_buf: HashMap::new(),
            recv_offset: 0,
            final_size: None,
            reset_error_code: None,
            send_offset: 0,
            fin_sent: false,
            acked_send_ranges: Vec::new(),
            fin_acked: false,
        }
    }

    pub(crate) fn acknowledge_sent_data(&mut self, offset: u64, len: usize, fin: bool) {
        let end = offset + len as u64;
        if offset < end {
            self.acked_send_ranges.push((offset, end));
            self.acked_send_ranges
                .sort_unstable_by_key(|(start, _)| *start);

            let mut merged = Vec::with_capacity(self.acked_send_ranges.len());
            for (range_start, range_end) in self.acked_send_ranges.drain(..) {
                if let Some((_, last_end)) = merged.last_mut() {
                    if range_start <= *last_end {
                        *last_end = (*last_end).max(range_end);
                        continue;
                    }
                }
                merged.push((range_start, range_end));
            }
            self.acked_send_ranges = merged;
        }

        if fin {
            self.fin_acked = true;
        }
    }

    pub(crate) fn all_sent_data_acked(&self) -> bool {
        if !self.fin_sent || !self.fin_acked {
            return false;
        }

        if self.send_offset == 0 {
            return true;
        }

        let mut covered_end = 0u64;
        for (range_start, range_end) in &self.acked_send_ranges {
            if *range_start > covered_end {
                return false;
            }
            covered_end = covered_end.max(*range_end);
            if covered_end >= self.send_offset {
                return true;
            }
        }

        false
    }
}

/// Extract stream properties from a stream ID.
pub fn stream_initiator(id: StreamId) -> StreamInitiator {
    if id & 0x01 == 0 {
        StreamInitiator::Client
    } else {
        StreamInitiator::Server
    }
}

pub fn stream_direction(id: StreamId) -> StreamDirection {
    if id & 0x02 == 0 {
        StreamDirection::Bidirectional
    } else {
        StreamDirection::Unidirectional
    }
}

/// Which side of the connection we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Client,
    Server,
}

/// Manages all streams for a QUIC connection.
#[derive(Debug)]
pub struct StreamManager {
    streams: HashMap<StreamId, StreamState>,
    side: Side,
    /// Next stream ID for locally-initiated bidirectional streams.
    next_bidi_id: StreamId,
    /// Next stream ID for locally-initiated unidirectional streams.
    next_uni_id: StreamId,
    /// Maximum number of bidirectional streams the peer allows.
    max_bidi_streams: u64,
    /// Maximum number of unidirectional streams the peer allows.
    max_uni_streams: u64,
    /// Number of locally-initiated bidirectional streams opened.
    bidi_opened: u64,
    /// Number of locally-initiated unidirectional streams opened.
    uni_opened: u64,
    /// Maximum number of remotely-initiated bidirectional streams we allow.
    local_max_bidi_streams: u64,
    /// Maximum number of remotely-initiated unidirectional streams we allow.
    local_max_uni_streams: u64,
    /// Largest MAX_STREAMS value already advertised for remotely-initiated bidirectional streams.
    advertised_local_max_bidi_streams: u64,
    /// Largest MAX_STREAMS value already advertised for remotely-initiated unidirectional streams.
    advertised_local_max_uni_streams: u64,
    /// Number of remotely-initiated bidirectional streams opened so far.
    remote_bidi_opened: u64,
    /// Number of remotely-initiated unidirectional streams opened so far.
    remote_uni_opened: u64,
    /// Our receive limit for locally-initiated bidirectional streams.
    local_bidi_local_recv_max: u64,
    /// Our receive limit for remotely-initiated bidirectional streams.
    local_bidi_remote_recv_max: u64,
    /// Our receive limit for remotely-initiated unidirectional streams.
    local_uni_recv_max: u64,
    /// The peer's receive limit for remotely-initiated bidirectional streams.
    peer_bidi_local_send_max: u64,
    /// The peer's receive limit for locally-initiated bidirectional streams.
    peer_bidi_remote_send_max: u64,
    /// The peer's receive limit for locally-initiated unidirectional streams.
    peer_uni_send_max: u64,
}

impl StreamManager {
    pub fn new(side: Side, max_bidi: u64, max_uni: u64, initial_flow: u64) -> Self {
        let (next_bidi, next_uni) = match side {
            Side::Client => (0, 2), // client bidi=0,4,8,... uni=2,6,10,...
            Side::Server => (1, 3), // server bidi=1,5,9,... uni=3,7,11,...
        };
        Self {
            streams: HashMap::new(),
            side,
            next_bidi_id: next_bidi,
            next_uni_id: next_uni,
            max_bidi_streams: max_bidi,
            max_uni_streams: max_uni,
            bidi_opened: 0,
            uni_opened: 0,
            local_max_bidi_streams: max_bidi,
            local_max_uni_streams: max_uni,
            advertised_local_max_bidi_streams: max_bidi,
            advertised_local_max_uni_streams: max_uni,
            remote_bidi_opened: 0,
            remote_uni_opened: 0,
            local_bidi_local_recv_max: initial_flow,
            local_bidi_remote_recv_max: initial_flow,
            local_uni_recv_max: initial_flow,
            peer_bidi_local_send_max: initial_flow,
            peer_bidi_remote_send_max: initial_flow,
            peer_uni_send_max: initial_flow,
        }
    }

    fn is_local_initiated(&self, id: StreamId) -> bool {
        matches!(
            (self.side, stream_initiator(id)),
            (Side::Client, StreamInitiator::Client) | (Side::Server, StreamInitiator::Server)
        )
    }

    fn remote_stream_base(&self, direction: StreamDirection) -> StreamId {
        match (self.side, direction) {
            (Side::Client, StreamDirection::Bidirectional) => 1,
            (Side::Client, StreamDirection::Unidirectional) => 3,
            (Side::Server, StreamDirection::Bidirectional) => 0,
            (Side::Server, StreamDirection::Unidirectional) => 2,
        }
    }

    fn initial_limits_for_stream(&self, id: StreamId, is_local: bool) -> (u64, u64) {
        match (stream_direction(id), is_local) {
            (StreamDirection::Bidirectional, true) => (
                self.peer_bidi_remote_send_max,
                self.local_bidi_local_recv_max,
            ),
            (StreamDirection::Bidirectional, false) => (
                self.peer_bidi_local_send_max,
                self.local_bidi_remote_recv_max,
            ),
            (StreamDirection::Unidirectional, true) => (self.peer_uni_send_max, 0),
            (StreamDirection::Unidirectional, false) => (0, self.local_uni_recv_max),
        }
    }

    /// Apply the peer's initial stream limits from transport parameters.
    pub fn apply_peer_initial_limits(
        &mut self,
        bidi_local_send_max: u64,
        bidi_remote_send_max: u64,
        uni_send_max: u64,
        max_bidi: u64,
        max_uni: u64,
    ) {
        self.peer_bidi_local_send_max = bidi_local_send_max;
        self.peer_bidi_remote_send_max = bidi_remote_send_max;
        self.peer_uni_send_max = uni_send_max;
        self.max_bidi_streams = max_bidi;
        self.max_uni_streams = max_uni;

        let side = self.side;
        let peer_bidi_local_send_max = self.peer_bidi_local_send_max;
        let peer_bidi_remote_send_max = self.peer_bidi_remote_send_max;
        let peer_uni_send_max = self.peer_uni_send_max;

        for (id, stream) in &mut self.streams {
            let is_local = matches!(
                (side, stream_initiator(*id)),
                (Side::Client, StreamInitiator::Client) | (Side::Server, StreamInitiator::Server)
            );
            let send_max = match (stream_direction(*id), is_local) {
                (StreamDirection::Bidirectional, true) => peer_bidi_remote_send_max,
                (StreamDirection::Bidirectional, false) => peer_bidi_local_send_max,
                (StreamDirection::Unidirectional, true) => peer_uni_send_max,
                (StreamDirection::Unidirectional, false) => 0,
            };
            stream.flow_control.set_send_max(send_max);
        }
    }

    /// Open a new locally-initiated bidirectional stream.
    pub fn open_bidi(&mut self) -> Result<StreamId, QuicError> {
        if self.bidi_opened >= self.max_bidi_streams {
            return Err(QuicError::Transport(
                TransportErrorCode::StreamLimitError,
                "bidirectional stream limit reached".into(),
            ));
        }
        let id = self.next_bidi_id;
        self.next_bidi_id += 4;
        self.bidi_opened += 1;
        self.streams.insert(
            id,
            StreamState::new(
                id,
                StreamDirection::Bidirectional,
                true,
                self.peer_bidi_remote_send_max,
                self.local_bidi_local_recv_max,
            ),
        );
        Ok(id)
    }

    /// Open a new locally-initiated unidirectional stream.
    pub fn open_uni(&mut self) -> Result<StreamId, QuicError> {
        if self.uni_opened >= self.max_uni_streams {
            return Err(QuicError::Transport(
                TransportErrorCode::StreamLimitError,
                "unidirectional stream limit reached".into(),
            ));
        }
        let id = self.next_uni_id;
        self.next_uni_id += 4;
        self.uni_opened += 1;
        self.streams.insert(
            id,
            StreamState::new(
                id,
                StreamDirection::Unidirectional,
                true,
                self.peer_uni_send_max,
                0,
            ),
        );
        Ok(id)
    }

    /// Accept a remotely-initiated stream (created implicitly by receiving a STREAM frame).
    pub fn accept_stream(&mut self, id: StreamId) -> Result<(), QuicError> {
        if self.streams.contains_key(&id) {
            return Ok(());
        }

        if self.is_local_initiated(id) {
            return Err(QuicError::Transport(
                TransportErrorCode::StreamStateError,
                format!("stream {id} is locally initiated"),
            ));
        }

        let direction = stream_direction(id);
        let base = self.remote_stream_base(direction);
        if id % 4 != base % 4 {
            return Err(QuicError::Transport(
                TransportErrorCode::StreamStateError,
                format!(
                    "stream {id} does not match remote {:?} stream numbering",
                    direction
                ),
            ));
        }
        let opened = id
            .checked_sub(base)
            .map(|ordinal| ordinal / 4 + 1)
            .ok_or_else(|| {
                QuicError::Transport(
                    TransportErrorCode::StreamStateError,
                    format!(
                        "stream {id} is below remote {:?} stream base {base}",
                        direction
                    ),
                )
            })?;
        let (limit, already_opened) = match direction {
            StreamDirection::Bidirectional => {
                (self.local_max_bidi_streams, self.remote_bidi_opened)
            }
            StreamDirection::Unidirectional => (self.local_max_uni_streams, self.remote_uni_opened),
        };

        if opened > limit {
            return Err(QuicError::Transport(
                TransportErrorCode::StreamLimitError,
                format!(
                    "stream {id} exceeds remote {:?} stream limit {limit}",
                    direction
                ),
            ));
        }

        if opened <= already_opened {
            return Ok(());
        }

        for index in already_opened..opened {
            let stream_id = base + index * 4;
            let (initial_send_max, initial_recv_max) =
                self.initial_limits_for_stream(stream_id, false);
            self.streams.insert(
                stream_id,
                StreamState::new(
                    stream_id,
                    direction,
                    false,
                    initial_send_max,
                    initial_recv_max,
                ),
            );
        }

        match direction {
            StreamDirection::Bidirectional => self.remote_bidi_opened = opened,
            StreamDirection::Unidirectional => self.remote_uni_opened = opened,
        }

        Ok(())
    }

    /// Get a stream by ID.
    pub fn get(&self, id: StreamId) -> Option<&StreamState> {
        self.streams.get(&id)
    }

    /// Get a mutable reference to a stream.
    pub fn get_mut(&mut self, id: StreamId) -> Option<&mut StreamState> {
        self.streams.get_mut(&id)
    }

    /// Update the peer's MAX_STREAMS for bidirectional streams.
    pub fn update_max_bidi_streams(&mut self, max: u64) {
        if max > self.max_bidi_streams {
            self.max_bidi_streams = max;
        }
    }

    /// Update the peer's MAX_STREAMS for unidirectional streams.
    pub fn update_max_uni_streams(&mut self, max: u64) {
        if max > self.max_uni_streams {
            self.max_uni_streams = max;
        }
    }

    /// Take MAX_STREAMS updates that should be advertised to the peer.
    pub fn take_max_streams_updates(&mut self) -> (Option<u64>, Option<u64>) {
        let bidi = (self.local_max_bidi_streams > self.advertised_local_max_bidi_streams)
            .then_some(self.local_max_bidi_streams);
        let uni = (self.local_max_uni_streams > self.advertised_local_max_uni_streams)
            .then_some(self.local_max_uni_streams);

        if let Some(max) = bidi {
            self.advertised_local_max_bidi_streams = max;
        }
        if let Some(max) = uni {
            self.advertised_local_max_uni_streams = max;
        }

        (bidi, uni)
    }

    pub fn local_max_streams(&self, bidirectional: bool) -> u64 {
        if bidirectional {
            self.local_max_bidi_streams
        } else {
            self.local_max_uni_streams
        }
    }

    /// Iterate over all stream IDs.
    pub fn stream_ids(&self) -> impl Iterator<Item = &StreamId> {
        self.streams.keys()
    }

    /// Find a remotely-initiated bidirectional stream that has received data.
    /// Used by servers to discover incoming request streams.
    pub fn find_incoming_bidi(&self) -> Option<StreamId> {
        let remote_initiator = match self.side {
            Side::Client => 1u64, // server-initiated bidi: bit 0 = 1, bit 1 = 0
            Side::Server => 0u64, // client-initiated bidi: bit 0 = 0, bit 1 = 0
        };

        self.streams
            .iter()
            .filter(|(id, state)| {
                // Remote-initiated bidirectional stream
                **id & 0x01 == remote_initiator && **id & 0x02 == 0 && !state.recv_buf.is_empty()
            })
            .map(|(id, _)| *id)
            .next()
    }

    /// Find a remotely-initiated unidirectional stream that has received data.
    /// Used to discover incoming control, push, and QPACK streams.
    pub fn find_incoming_uni(&self) -> Option<StreamId> {
        self.incoming_uni_ids().into_iter().next()
    }

    /// Collect remotely-initiated unidirectional streams that currently have
    /// buffered receive data.
    pub fn incoming_uni_ids(&self) -> Vec<StreamId> {
        let remote_initiator = match self.side {
            Side::Client => 1u64, // server-initiated uni: bit 0 = 1, bit 1 = 1
            Side::Server => 0u64, // client-initiated uni: bit 0 = 0, bit 1 = 1
        };

        let mut incoming: Vec<StreamId> = self
            .streams
            .iter()
            .filter(|(id, state)| {
                // Remote-initiated unidirectional stream
                **id & 0x01 == remote_initiator && **id & 0x02 == 0x02 && !state.recv_buf.is_empty()
            })
            .map(|(id, _)| *id)
            .collect();
        incoming.sort_unstable();
        incoming
    }

    /// Number of active streams.
    pub fn active_count(&self) -> usize {
        self.streams.len()
    }

    /// Remove streams that are fully closed (both send and recv in terminal states).
    /// Returns the number of streams removed.
    pub fn gc_closed_streams(&mut self) -> usize {
        let before = self.streams.len();
        let side = self.side;
        let mut retired_remote_bidi = 0u64;
        let mut retired_remote_uni = 0u64;
        self.streams.retain(|id, stream| {
            let send_done = matches!(
                stream.send,
                Some(SendState::DataRecvd | SendState::ResetRecvd) | None
            );
            let recv_done = matches!(
                stream.recv,
                Some(RecvState::DataRead | RecvState::ResetRead) | None
            );
            let keep = !(send_done && recv_done);
            if !keep {
                let is_local = matches!(
                    (side, stream_initiator(*id)),
                    (Side::Client, StreamInitiator::Client)
                        | (Side::Server, StreamInitiator::Server)
                );
                if !is_local {
                    match stream_direction(*id) {
                        StreamDirection::Bidirectional => retired_remote_bidi += 1,
                        StreamDirection::Unidirectional => retired_remote_uni += 1,
                    }
                }
            }
            keep
        });
        self.local_max_bidi_streams = self
            .local_max_bidi_streams
            .saturating_add(retired_remote_bidi);
        self.local_max_uni_streams = self
            .local_max_uni_streams
            .saturating_add(retired_remote_uni);
        before - self.streams.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_id_properties() {
        // Client bidi: 0, 4, 8, ...
        assert_eq!(stream_initiator(0), StreamInitiator::Client);
        assert_eq!(stream_direction(0), StreamDirection::Bidirectional);

        // Server bidi: 1, 5, 9, ...
        assert_eq!(stream_initiator(1), StreamInitiator::Server);
        assert_eq!(stream_direction(1), StreamDirection::Bidirectional);

        // Client uni: 2, 6, 10, ...
        assert_eq!(stream_initiator(2), StreamInitiator::Client);
        assert_eq!(stream_direction(2), StreamDirection::Unidirectional);

        // Server uni: 3, 7, 11, ...
        assert_eq!(stream_initiator(3), StreamInitiator::Server);
        assert_eq!(stream_direction(3), StreamDirection::Unidirectional);
    }

    #[test]
    fn client_opens_bidi_streams() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        let id1 = mgr.open_bidi().unwrap();
        assert_eq!(id1, 0); // first client bidi
        let id2 = mgr.open_bidi().unwrap();
        assert_eq!(id2, 4); // second client bidi

        assert!(mgr.get(0).is_some());
        assert!(mgr.get(4).is_some());
        assert_eq!(mgr.active_count(), 2);
    }

    #[test]
    fn server_opens_uni_streams() {
        let mut mgr = StreamManager::new(Side::Server, 10, 10, 65536);

        let id1 = mgr.open_uni().unwrap();
        assert_eq!(id1, 3); // first server uni
        let id2 = mgr.open_uni().unwrap();
        assert_eq!(id2, 7); // second server uni
    }

    #[test]
    fn stream_limit_error() {
        let mut mgr = StreamManager::new(Side::Client, 1, 0, 65536);

        mgr.open_bidi().unwrap();
        assert!(mgr.open_bidi().is_err());
    }

    #[test]
    fn accept_remote_stream() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        // Server-initiated bidi stream
        mgr.accept_stream(1).unwrap();
        let stream = mgr.get(1).unwrap();
        assert!(stream.send.is_some());
        assert!(stream.recv.is_some());
    }

    #[test]
    fn accept_remote_uni_stream() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        // Server-initiated uni stream — client can only receive
        mgr.accept_stream(3).unwrap();
        let stream = mgr.get(3).unwrap();
        assert!(stream.send.is_none()); // can't send on remote uni
        assert!(stream.recv.is_some()); // can receive
    }

    #[test]
    fn gc_closed_streams_removes_terminal() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        let id = mgr.open_bidi().unwrap();
        assert_eq!(mgr.active_count(), 1);

        // Transition send to terminal state
        let stream = mgr.get_mut(id).unwrap();
        stream.send = Some(SendState::DataRecvd);
        stream.recv = Some(RecvState::DataRead);

        let removed = mgr.gc_closed_streams();
        assert_eq!(removed, 1);
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn gc_closed_streams_keeps_active() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        let id = mgr.open_bidi().unwrap();
        // Stream is still active (Ready/Recv states)
        let removed = mgr.gc_closed_streams();
        assert_eq!(removed, 0);
        assert_eq!(mgr.active_count(), 1);
        assert!(mgr.get(id).is_some());
    }

    #[test]
    fn gc_closed_streams_reset_states() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        let id = mgr.open_bidi().unwrap();
        let stream = mgr.get_mut(id).unwrap();
        stream.send = Some(SendState::ResetRecvd);
        stream.recv = Some(RecvState::ResetRead);

        let removed = mgr.gc_closed_streams();
        assert_eq!(removed, 1);
    }

    #[test]
    fn gc_closed_remote_stream_replenishes_max_streams() {
        let mut mgr = StreamManager::new(Side::Client, 2, 2, 65536);

        mgr.accept_stream(1).unwrap();
        {
            let stream = mgr.get_mut(1).unwrap();
            stream.send = Some(SendState::DataRecvd);
            stream.recv = Some(RecvState::DataRead);
        }

        assert_eq!(mgr.gc_closed_streams(), 1);
        assert_eq!(mgr.take_max_streams_updates(), (Some(3), None));
        assert_eq!(mgr.take_max_streams_updates(), (None, None));
    }

    #[test]
    fn stream_state_transitions() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        let id = mgr.open_bidi().unwrap();
        let stream = mgr.get(id).unwrap();
        assert_eq!(stream.send, Some(SendState::Ready));
        assert_eq!(stream.recv, Some(RecvState::Recv));
    }

    #[test]
    fn apply_peer_initial_limits_updates_existing_and_future_streams() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);
        let local_bidi = mgr.open_bidi().unwrap();
        let local_uni = mgr.open_uni().unwrap();
        mgr.accept_stream(1).unwrap(); // server-initiated bidi

        mgr.apply_peer_initial_limits(111, 222, 333, 4, 5);

        assert_eq!(mgr.max_bidi_streams, 4);
        assert_eq!(mgr.max_uni_streams, 5);
        assert_eq!(mgr.get(local_bidi).unwrap().flow_control.send_window(), 222);
        assert_eq!(mgr.get(local_uni).unwrap().flow_control.send_window(), 333);
        assert_eq!(mgr.get(1).unwrap().flow_control.send_window(), 111);

        let new_local_bidi = mgr.open_bidi().unwrap();
        assert_eq!(
            mgr.get(new_local_bidi).unwrap().flow_control.send_window(),
            222
        );
    }

    #[test]
    fn accept_stream_creates_implicit_lower_remote_streams() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        mgr.accept_stream(9).unwrap();

        assert!(mgr.get(1).is_some());
        assert!(mgr.get(5).is_some());
        assert!(mgr.get(9).is_some());
    }

    #[test]
    fn accept_stream_enforces_remote_stream_limit() {
        let mut mgr = StreamManager::new(Side::Client, 1, 10, 65536);

        let err = mgr.accept_stream(5).unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::StreamLimitError, _)
        ));
    }

    #[test]
    fn accept_stream_rejects_locally_initiated_id() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        let err = mgr.accept_stream(0).unwrap_err();
        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::StreamStateError, _)
        ));
    }

    #[test]
    fn accept_stream_ignores_retransmit_for_gced_remote_stream() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        mgr.accept_stream(1).unwrap();
        {
            let stream = mgr.get_mut(1).unwrap();
            stream.send = Some(SendState::DataRecvd);
            stream.recv = Some(RecvState::DataRead);
        }
        mgr.gc_closed_streams();

        assert!(mgr.accept_stream(1).is_ok());
        assert!(mgr.get(1).is_none());
    }

    #[test]
    fn incoming_uni_ids_returns_all_remote_uni_streams_with_data() {
        let mut mgr = StreamManager::new(Side::Client, 10, 10, 65536);

        mgr.accept_stream(3).unwrap();
        mgr.accept_stream(7).unwrap();
        mgr.accept_stream(11).unwrap();
        mgr.get_mut(3)
            .unwrap()
            .recv_buf
            .insert(0, b"control".to_vec());
        mgr.get_mut(7)
            .unwrap()
            .recv_buf
            .insert(0, b"encoder".to_vec());

        let incoming = mgr.incoming_uni_ids();

        assert_eq!(incoming, vec![3, 7]);
        assert_eq!(mgr.find_incoming_uni(), Some(3));
    }
}
