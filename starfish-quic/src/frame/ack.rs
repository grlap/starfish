//! ACK frame encoding and decoding (RFC 9000 §19.3).
//!
//! ACK frames report which packets have been received. They carry a set of
//! acknowledged packet number ranges and an optional ECN section.

use crate::error::QuicError;
use crate::packet::{decode_varint, encode_varint};

/// An ACK frame (RFC 9000 §19.3).
#[derive(Debug, Clone)]
pub struct AckFrame {
    /// The largest packet number being acknowledged.
    pub largest_acknowledged: u64,
    /// ACK delay in microseconds (encoded as multiples of 2^ack_delay_exponent).
    pub ack_delay: u64,
    /// Acknowledged ranges as (start, end) inclusive pairs in descending order.
    pub ranges: Vec<AckRange>,
    /// ECN counts, if present (ACK_ECN frame type 0x03).
    pub ecn: Option<EcnCounts>,
}

/// A contiguous range of acknowledged packet numbers.
#[derive(Debug, Clone, Copy)]
pub struct AckRange {
    /// Smallest packet number in this range.
    pub start: u64,
    /// Largest packet number in this range.
    pub end: u64,
}

/// ECN (Explicit Congestion Notification) counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EcnCounts {
    pub ect0: u64,
    pub ect1: u64,
    pub ce: u64,
}

impl AckFrame {
    /// Encode into a buffer. Uses frame type 0x02 (or 0x03 if ECN present).
    pub fn encode(&self, buf: &mut Vec<u8>) -> Result<(), QuicError> {
        let frame_type = if self.ecn.is_some() { 0x03u64 } else { 0x02u64 };
        encode_varint(frame_type, buf)?;

        encode_varint(self.largest_acknowledged, buf)?;
        encode_varint(self.ack_delay, buf)?;

        // ACK Range Count
        let range_count = if self.ranges.is_empty() {
            0u64
        } else {
            (self.ranges.len() - 1) as u64
        };
        encode_varint(range_count, buf)?;

        // First ACK Range (number of packets - 1 from largest_acknowledged)
        if let Some(first) = self.ranges.first() {
            let first_ack_range = self.largest_acknowledged.saturating_sub(first.start);
            encode_varint(first_ack_range, buf)?;

            // Additional ranges
            let mut prev_smallest = first.start;
            for range in &self.ranges[1..] {
                // Gap: number of unacknowledged packets - 1
                let gap = prev_smallest.saturating_sub(range.end).saturating_sub(2);
                encode_varint(gap, buf)?;

                // ACK Range: number of packets - 1
                let ack_range = range.end.saturating_sub(range.start);
                encode_varint(ack_range, buf)?;

                prev_smallest = range.start;
            }
        } else {
            encode_varint(0, buf)?; // first ack range = 0
        }

        // ECN counts
        if let Some(ecn) = &self.ecn {
            encode_varint(ecn.ect0, buf)?;
            encode_varint(ecn.ect1, buf)?;
            encode_varint(ecn.ce, buf)?;
        }

        Ok(())
    }
}

/// Decode an ACK frame. `has_ecn` is true for type 0x03.
pub fn decode_ack_frame(buf: &[u8], has_ecn: bool) -> Result<(AckFrame, usize), QuicError> {
    let mut offset = 0;

    let (largest_ack, n) = decode_varint(&buf[offset..])?;
    offset += n;

    let (ack_delay, n) = decode_varint(&buf[offset..])?;
    offset += n;

    let (range_count, n) = decode_varint(&buf[offset..])?;
    offset += n;

    let (first_ack_range, n) = decode_varint(&buf[offset..])?;
    offset += n;

    let mut ranges = Vec::new();

    // First range
    let first_start = largest_ack
        .checked_sub(first_ack_range)
        .ok_or_else(|| QuicError::InvalidFrame("ACK first range underflow".into()))?;
    ranges.push(AckRange {
        start: first_start,
        end: largest_ack,
    });

    let mut smallest = first_start;

    // Additional ranges
    for _ in 0..range_count {
        let (gap, n) = decode_varint(&buf[offset..])?;
        offset += n;

        let (ack_range, n) = decode_varint(&buf[offset..])?;
        offset += n;

        let gap_to_next_range = gap
            .checked_add(2)
            .ok_or_else(|| QuicError::InvalidFrame("ACK range gap overflow".into()))?;
        let end = smallest
            .checked_sub(gap_to_next_range)
            .ok_or_else(|| QuicError::InvalidFrame("ACK range gap underflow".into()))?;
        let start = end
            .checked_sub(ack_range)
            .ok_or_else(|| QuicError::InvalidFrame("ACK range underflow".into()))?;

        ranges.push(AckRange { start, end });
        smallest = start;
    }

    let ecn = if has_ecn {
        let (ect0, n) = decode_varint(&buf[offset..])?;
        offset += n;
        let (ect1, n) = decode_varint(&buf[offset..])?;
        offset += n;
        let (ce, n) = decode_varint(&buf[offset..])?;
        offset += n;
        Some(EcnCounts { ect0, ect1, ce })
    } else {
        None
    };

    Ok((
        AckFrame {
            largest_acknowledged: largest_ack,
            ack_delay,
            ranges,
            ecn,
        },
        offset,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_range_roundtrip() {
        let ack = AckFrame {
            largest_acknowledged: 100,
            ack_delay: 500,
            ranges: vec![AckRange {
                start: 95,
                end: 100,
            }],
            ecn: None,
        };

        let mut buf = Vec::new();
        ack.encode(&mut buf).unwrap();

        // Skip the frame type varint
        let (frame_type, type_len) = decode_varint(&buf).unwrap();
        assert_eq!(frame_type, 0x02);

        let (decoded, _) = decode_ack_frame(&buf[type_len..], false).unwrap();
        assert_eq!(decoded.largest_acknowledged, 100);
        assert_eq!(decoded.ack_delay, 500);
        assert_eq!(decoded.ranges.len(), 1);
        assert_eq!(decoded.ranges[0].start, 95);
        assert_eq!(decoded.ranges[0].end, 100);
        assert!(decoded.ecn.is_none());
    }

    #[test]
    fn multiple_ranges_roundtrip() {
        let ack = AckFrame {
            largest_acknowledged: 100,
            ack_delay: 0,
            ranges: vec![
                AckRange {
                    start: 95,
                    end: 100,
                },
                AckRange { start: 80, end: 90 },
            ],
            ecn: None,
        };

        let mut buf = Vec::new();
        ack.encode(&mut buf).unwrap();

        let (_, type_len) = decode_varint(&buf).unwrap();
        let (decoded, _) = decode_ack_frame(&buf[type_len..], false).unwrap();
        assert_eq!(decoded.ranges.len(), 2);
        assert_eq!(decoded.ranges[0].start, 95);
        assert_eq!(decoded.ranges[0].end, 100);
        assert_eq!(decoded.ranges[1].start, 80);
        assert_eq!(decoded.ranges[1].end, 90);
    }

    #[test]
    fn encode_saturates_when_largest_below_start() {
        // Malformed: largest_acknowledged < first range start.
        // saturating_sub should prevent panic.
        let ack = AckFrame {
            largest_acknowledged: 0,
            ack_delay: 0,
            ranges: vec![AckRange { start: 5, end: 10 }],
            ecn: None,
        };

        let mut buf = Vec::new();
        ack.encode(&mut buf).unwrap(); // must not panic
        assert!(!buf.is_empty());
    }

    #[test]
    fn encode_saturates_overlapping_ranges() {
        // Malformed: second range end >= first range start, causing gap underflow.
        // saturating_sub should prevent panic.
        let ack = AckFrame {
            largest_acknowledged: 100,
            ack_delay: 0,
            ranges: vec![
                AckRange {
                    start: 95,
                    end: 100,
                },
                AckRange { start: 96, end: 98 },
            ],
            ecn: None,
        };

        let mut buf = Vec::new();
        ack.encode(&mut buf).unwrap(); // must not panic
        assert!(!buf.is_empty());
    }

    #[test]
    fn encode_boundary_zero_range() {
        // Boundary: single packet ack at packet number 0
        let ack = AckFrame {
            largest_acknowledged: 0,
            ack_delay: 0,
            ranges: vec![AckRange { start: 0, end: 0 }],
            ecn: None,
        };

        let mut buf = Vec::new();
        ack.encode(&mut buf).unwrap();

        let (_, type_len) = decode_varint(&buf).unwrap();
        let (decoded, _) = decode_ack_frame(&buf[type_len..], false).unwrap();
        assert_eq!(decoded.largest_acknowledged, 0);
        assert_eq!(decoded.ranges[0].start, 0);
        assert_eq!(decoded.ranges[0].end, 0);
    }

    #[test]
    fn decode_rejects_first_range_underflow() {
        // Craft bytes where first_ack_range > largest_acknowledged
        let mut buf = Vec::new();
        encode_varint(5, &mut buf).unwrap(); // largest_acknowledged = 5
        encode_varint(0, &mut buf).unwrap(); // ack_delay = 0
        encode_varint(0, &mut buf).unwrap(); // range_count = 0
        encode_varint(10, &mut buf).unwrap(); // first_ack_range = 10 (underflows: 5 - 10)

        let result = decode_ack_frame(&buf, false);
        assert!(result.is_err());
    }

    #[test]
    fn decode_rejects_huge_range_count_without_preallocating() {
        let mut buf = Vec::new();
        encode_varint(5, &mut buf).unwrap(); // largest_acknowledged = 5
        encode_varint(0, &mut buf).unwrap(); // ack_delay = 0
        encode_varint((1u64 << 62) - 1, &mut buf).unwrap(); // absurd range_count
        encode_varint(0, &mut buf).unwrap(); // first_ack_range = 0

        let result = decode_ack_frame(&buf, false);
        assert!(result.is_err());
    }

    #[test]
    fn ecn_roundtrip() {
        let ack = AckFrame {
            largest_acknowledged: 50,
            ack_delay: 100,
            ranges: vec![AckRange { start: 50, end: 50 }],
            ecn: Some(EcnCounts {
                ect0: 10,
                ect1: 5,
                ce: 1,
            }),
        };

        let mut buf = Vec::new();
        ack.encode(&mut buf).unwrap();

        let (frame_type, type_len) = decode_varint(&buf).unwrap();
        assert_eq!(frame_type, 0x03); // ECN variant

        let (decoded, _) = decode_ack_frame(&buf[type_len..], true).unwrap();
        let ecn = decoded.ecn.unwrap();
        assert_eq!(ecn.ect0, 10);
        assert_eq!(ecn.ect1, 5);
        assert_eq!(ecn.ce, 1);
    }
}
