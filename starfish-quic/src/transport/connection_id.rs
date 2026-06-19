//! Connection ID management (RFC 9000 §5.1).
//!
//! Each QUIC connection has one or more connection IDs. Peers can issue new
//! CIDs and retire old ones to support connection migration and linkability
//! resistance.

use crate::error::{QuicError, TransportErrorCode};
use crate::packet::ConnectionId;

/// A connection ID with its sequence number and stateless reset token.
#[derive(Debug, Clone)]
pub struct IssuedConnectionId {
    pub sequence_number: u64,
    pub cid: ConnectionId,
    pub stateless_reset_token: Option<[u8; 16]>,
    pub retired: bool,
    pub advertised_to_peer: bool,
}

/// Manages the set of connection IDs for one side of the connection.
#[derive(Debug)]
pub struct ConnectionIdManager {
    /// CIDs issued by us (local) for the peer to use as DCID.
    issued: Vec<IssuedConnectionId>,
    /// CIDs issued by the peer for us to use as DCID.
    peer: Vec<IssuedConnectionId>,
    /// The active CID we use as DCID when sending.
    active_peer_cid_index: usize,
    /// Next sequence number for issuing new CIDs.
    next_sequence: u64,
    /// The CID length we use.
    cid_len: u8,
    /// Largest Retire Prior To value received from the peer.
    max_peer_retire_prior_to: u64,
}

impl ConnectionIdManager {
    fn prune_retired_local_cids(&mut self) {
        self.issued.retain(|entry| !entry.retired);
    }

    fn retire_peer_cids_below(&mut self, retire_prior_to: u64) {
        for entry in &mut self.peer {
            if entry.sequence_number < retire_prior_to {
                entry.retired = true;
            }
        }
    }

    fn rotate_away_from_retired_active_peer_cid(&mut self) {
        if self.peer[self.active_peer_cid_index].retired {
            if let Some(new_index) = self.peer.iter().position(|entry| !entry.retired) {
                self.active_peer_cid_index = new_index;
            }
        }
    }

    pub fn new(initial_local_cid: ConnectionId, initial_peer_cid: ConnectionId) -> Self {
        let cid_len = initial_local_cid.len() as u8;
        Self {
            issued: vec![IssuedConnectionId {
                sequence_number: 0,
                cid: initial_local_cid,
                stateless_reset_token: None,
                retired: false,
                advertised_to_peer: true,
            }],
            peer: vec![IssuedConnectionId {
                sequence_number: 0,
                cid: initial_peer_cid,
                stateless_reset_token: None,
                retired: false,
                advertised_to_peer: true,
            }],
            active_peer_cid_index: 0,
            next_sequence: 1,
            cid_len,
            max_peer_retire_prior_to: 0,
        }
    }

    /// Issue a new local CID for the peer to use.
    pub fn issue_new_cid(&mut self) -> IssuedConnectionId {
        let cid = ConnectionId::generate(self.cid_len);
        let mut token = [0u8; 16];
        use rand::Rng;
        rand::rng().fill(&mut token);

        let entry = IssuedConnectionId {
            sequence_number: self.next_sequence,
            cid,
            stateless_reset_token: Some(token),
            retired: false,
            advertised_to_peer: false,
        };
        self.next_sequence += 1;
        self.issued.push(entry.clone());
        entry
    }

    /// Register a CID received from the peer (via NEW_CONNECTION_ID frame).
    pub fn add_peer_cid(
        &mut self,
        sequence_number: u64,
        cid: ConnectionId,
        stateless_reset_token: [u8; 16],
        retire_prior_to: u64,
    ) -> Result<(), QuicError> {
        let retire_prior_to = self.max_peer_retire_prior_to.max(retire_prior_to);
        self.max_peer_retire_prior_to = retire_prior_to;
        self.retire_peer_cids_below(retire_prior_to);

        if let Some(existing_index) = self
            .peer
            .iter()
            .position(|entry| entry.sequence_number == sequence_number)
        {
            let existing = &self.peer[existing_index];
            if existing.cid != cid || existing.stateless_reset_token != Some(stateless_reset_token)
            {
                return Err(QuicError::Transport(
                    TransportErrorCode::ProtocolViolation,
                    format!(
                        "NEW_CONNECTION_ID sequence {sequence_number} conflicts with existing CID"
                    ),
                ));
            }

            if sequence_number < retire_prior_to {
                self.peer[existing_index].retired = true;
            }
            self.rotate_away_from_retired_active_peer_cid();
            return Ok(());
        }

        self.peer.push(IssuedConnectionId {
            sequence_number,
            cid,
            stateless_reset_token: Some(stateless_reset_token),
            retired: sequence_number < retire_prior_to,
            advertised_to_peer: true,
        });

        self.rotate_away_from_retired_active_peer_cid();
        Ok(())
    }

    /// Retire a local CID by sequence number (peer sent RETIRE_CONNECTION_ID).
    pub fn retire_local_cid(&mut self, sequence_number: u64) {
        for entry in &mut self.issued {
            if entry.sequence_number == sequence_number {
                entry.retired = true;
            }
        }
        self.prune_retired_local_cids();
        if self.issued.is_empty() {
            self.issue_new_cid();
        }
    }

    /// Get the active peer CID to use as our DCID.
    pub fn active_peer_cid(&self) -> &ConnectionId {
        &self.peer[self.active_peer_cid_index].cid
    }

    /// Rotate to another non-retired peer CID, if one is available.
    pub fn rotate_active_peer_cid(&mut self) -> bool {
        if let Some((new_index, _)) = self
            .peer
            .iter()
            .enumerate()
            .filter(|(index, entry)| *index != self.active_peer_cid_index && !entry.retired)
            .max_by_key(|(_, entry)| entry.sequence_number)
        {
            self.active_peer_cid_index = new_index;
            true
        } else {
            false
        }
    }

    /// Get the primary (first non-retired) local CID.
    pub fn primary_local_cid(&self) -> &ConnectionId {
        self.issued
            .iter()
            .find(|e| !e.retired)
            .map(|e| &e.cid)
            .unwrap_or(&self.issued[0].cid)
    }

    /// Find a local CID by its bytes (for routing incoming packets).
    #[cfg(test)]
    pub fn find_local_cid(&self, cid_bytes: &[u8]) -> Option<&IssuedConnectionId> {
        self.issued
            .iter()
            .find(|e| !e.retired && e.cid.as_slice() == cid_bytes)
    }

    /// Get the CID length used by this connection.
    pub fn cid_len(&self) -> u8 {
        self.cid_len
    }

    /// Replace the active peer CID (used during Retry handling).
    ///
    /// The Retry SCID becomes the new peer CID. The old initial peer CID
    /// is discarded since Initial keys will be re-derived.
    pub fn set_peer_cid(&mut self, cid: ConnectionId) {
        self.peer[self.active_peer_cid_index].cid = cid;
    }

    /// Attach a stateless reset token to the primary local CID.
    pub fn set_primary_local_stateless_reset_token(&mut self, token: [u8; 16]) {
        self.issued[0].stateless_reset_token = Some(token);
    }

    /// Attach a stateless reset token to the active peer CID.
    pub fn set_active_peer_stateless_reset_token(&mut self, token: [u8; 16]) {
        self.peer[self.active_peer_cid_index].stateless_reset_token = Some(token);
    }

    /// Check if a 16-byte token matches any known peer stateless reset token.
    ///
    /// Used to detect stateless reset packets (RFC 9000 §10.3).
    pub fn is_peer_stateless_reset_token(&self, token: &[u8; 16]) -> bool {
        self.peer
            .iter()
            .any(|e| e.stateless_reset_token.as_ref() == Some(token))
    }

    /// Count the non-retired local CIDs we currently advertise.
    pub fn active_local_cid_count(&self) -> usize {
        self.issued.iter().filter(|entry| !entry.retired).count()
    }

    /// The smallest sequence number that is still active locally.
    pub fn local_retire_prior_to(&self) -> u64 {
        self.issued
            .iter()
            .filter(|entry| !entry.retired)
            .map(|entry| entry.sequence_number)
            .min()
            .unwrap_or(0)
    }

    /// Take local CIDs that still need a NEW_CONNECTION_ID frame.
    pub fn take_unadvertised_local_cids(&mut self) -> Vec<IssuedConnectionId> {
        let mut pending = Vec::new();
        for entry in &mut self.issued {
            if !entry.retired && !entry.advertised_to_peer {
                entry.advertised_to_peer = true;
                pending.push(entry.clone());
            }
        }
        self.prune_retired_local_cids();
        pending
    }

    /// Take peer CIDs that need RETIRE_CONNECTION_ID frames sent.
    pub fn take_peer_cids_to_retire(&mut self) -> Vec<u64> {
        let active_seq = self.peer[self.active_peer_cid_index].sequence_number;
        let mut retire = Vec::new();
        let mut retained = Vec::with_capacity(self.peer.len());
        let mut new_active_index = 0usize;

        for entry in self.peer.drain(..) {
            if entry.retired && entry.sequence_number != active_seq {
                retire.push(entry.sequence_number);
                continue;
            }

            if entry.sequence_number == active_seq {
                new_active_index = retained.len();
            }
            retained.push(entry);
        }

        self.peer = retained;
        self.active_peer_cid_index = new_active_index;
        retire
    }

    /// Iterate over non-retired local CIDs.
    pub fn local_cids(&self) -> impl Iterator<Item = &IssuedConnectionId> {
        self.issued.iter().filter(|entry| !entry.retired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_lifecycle() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);

        let mgr = ConnectionIdManager::new(local.clone(), peer.clone());
        assert_eq!(mgr.active_peer_cid().as_slice(), peer.as_slice());
        assert_eq!(mgr.cid_len(), 4);
        assert!(mgr.find_local_cid(local.as_slice()).is_some());
    }

    #[test]
    fn issue_new_cid() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);

        let new_cid = mgr.issue_new_cid();
        assert_eq!(new_cid.sequence_number, 1);
        assert_eq!(new_cid.cid.len(), 4);
        assert!(new_cid.stateless_reset_token.is_some());
        assert!(!new_cid.advertised_to_peer);
    }

    #[test]
    fn retire_local_cid() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local.clone(), peer);

        mgr.retire_local_cid(0);
        assert!(mgr.find_local_cid(local.as_slice()).is_none());
    }

    #[test]
    fn add_and_retire_peer_cids() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);

        let new_peer_cid = ConnectionId::from_slice(&[9, 10, 11, 12]);
        mgr.add_peer_cid(1, new_peer_cid, [0xbb; 16], 1).unwrap();

        // Original CID (seq 0) should be retired
        let to_retire = mgr.take_peer_cids_to_retire();
        assert_eq!(to_retire, vec![0]);
        assert_eq!(mgr.active_peer_cid().as_slice(), &[9, 10, 11, 12]);
    }

    #[test]
    fn duplicate_peer_sequence_numbers_are_ignored() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);

        mgr.add_peer_cid(1, ConnectionId::from_slice(&[9, 10, 11, 12]), [0xbb; 16], 0)
            .unwrap();
        mgr.add_peer_cid(1, ConnectionId::from_slice(&[9, 10, 11, 12]), [0xbb; 16], 1)
            .unwrap();

        assert_eq!(mgr.peer.len(), 2);
        assert_eq!(mgr.peer[1].cid.as_slice(), &[9, 10, 11, 12]);
        assert_eq!(mgr.active_peer_cid().as_slice(), &[9, 10, 11, 12]);
    }

    #[test]
    fn conflicting_duplicate_peer_sequence_numbers_error() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);

        mgr.add_peer_cid(1, ConnectionId::from_slice(&[9, 10, 11, 12]), [0xbb; 16], 0)
            .unwrap();
        let err = mgr
            .add_peer_cid(
                1,
                ConnectionId::from_slice(&[13, 14, 15, 16]),
                [0xcc; 16],
                0,
            )
            .unwrap_err();

        assert!(matches!(
            err,
            QuicError::Transport(TransportErrorCode::ProtocolViolation, _)
        ));
    }

    #[test]
    fn set_primary_local_stateless_reset_token() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);

        let token = [0xcc; 16];
        mgr.set_primary_local_stateless_reset_token(token);

        assert_eq!(mgr.issued[0].stateless_reset_token, Some(token));
    }

    #[test]
    fn take_unadvertised_local_cids_marks_entries_advertised() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);

        let issued = mgr.issue_new_cid();
        let pending = mgr.take_unadvertised_local_cids();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].sequence_number, issued.sequence_number);
        assert!(mgr.take_unadvertised_local_cids().is_empty());
    }

    #[test]
    fn retire_local_cid_prunes_retired_entries() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);

        let seq1 = mgr.issue_new_cid().sequence_number;
        let seq2 = mgr.issue_new_cid().sequence_number;

        mgr.retire_local_cid(0);
        mgr.retire_local_cid(seq1);

        assert_eq!(mgr.issued.len(), 1);
        assert_eq!(mgr.issued[0].sequence_number, seq2);
        assert!(!mgr.issued[0].retired);
    }

    #[test]
    fn rotate_active_peer_cid_switches_to_another_advertised_peer_id() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);
        let original = mgr.active_peer_cid().clone();

        let replacement = ConnectionId::from_slice(&[9, 10, 11, 12]);
        mgr.add_peer_cid(1, replacement.clone(), [0xdd; 16], 0)
            .unwrap();

        assert!(mgr.rotate_active_peer_cid());
        assert_eq!(mgr.active_peer_cid().as_slice(), replacement.as_slice());
        assert!(mgr.rotate_active_peer_cid());
        assert_eq!(mgr.active_peer_cid().as_slice(), original.as_slice());
    }

    #[test]
    fn rotate_active_peer_cid_falls_back_to_older_advertised_id() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);

        let older = ConnectionId::from_slice(&[9, 10, 11, 12]);
        let newest = ConnectionId::from_slice(&[13, 14, 15, 16]);
        mgr.add_peer_cid(1, older.clone(), [0xdd; 16], 0).unwrap();
        mgr.add_peer_cid(2, newest.clone(), [0xee; 16], 0).unwrap();

        assert!(mgr.rotate_active_peer_cid());
        assert_eq!(mgr.active_peer_cid().as_slice(), newest.as_slice());
        assert!(mgr.rotate_active_peer_cid());
        assert_eq!(mgr.active_peer_cid().as_slice(), older.as_slice());
    }

    #[test]
    fn retire_only_local_cid_immediately_issues_replacement() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local.clone(), peer);

        mgr.retire_local_cid(0);

        assert_eq!(mgr.active_local_cid_count(), 1);
        assert_ne!(mgr.primary_local_cid().as_slice(), local.as_slice());
        assert_eq!(mgr.issued.len(), 1);
        assert_eq!(mgr.issued[0].sequence_number, 1);
        assert!(!mgr.issued[0].retired);
        assert!(!mgr.issued[0].advertised_to_peer);
    }

    #[test]
    fn add_peer_cid_tracks_highest_retire_prior_to_across_reordering() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);

        mgr.add_peer_cid(5, ConnectionId::from_slice(&[9, 10, 11, 12]), [0xaa; 16], 5)
            .unwrap();
        mgr.add_peer_cid(
            3,
            ConnectionId::from_slice(&[13, 14, 15, 16]),
            [0xbb; 16],
            3,
        )
        .unwrap();

        let to_retire = mgr.take_peer_cids_to_retire();
        assert_eq!(to_retire, vec![0, 3]);
        assert_eq!(mgr.active_peer_cid().as_slice(), &[9, 10, 11, 12]);
    }

    #[test]
    fn local_retire_prior_to_advances_after_local_cid_retirement() {
        let local = ConnectionId::from_slice(&[1, 2, 3, 4]);
        let peer = ConnectionId::from_slice(&[5, 6, 7, 8]);
        let mut mgr = ConnectionIdManager::new(local, peer);

        mgr.issue_new_cid();
        mgr.retire_local_cid(0);

        assert_eq!(mgr.local_retire_prior_to(), 1);
    }
}
