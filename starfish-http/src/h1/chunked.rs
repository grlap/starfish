//! Chunked transfer-encoding codec (RFC 9112 §7.1).
//!
//! Provides [`ChunkedEncoder`] for wrapping data into chunked format and
//! [`ChunkedDecoder`] for incrementally decoding chunked streams.

use super::strip_line_ending;
use crate::error::HttpError;
use std::fmt::Write as FmtWrite;

/// Encode a single chunk in chunked transfer-encoding format.
///
/// Returns `"{size_hex}\r\n{data}\r\n"`.
pub fn encode_chunk(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return b"0\r\n\r\n".to_vec();
    }

    let mut buf = String::new();
    let _ = write!(buf, "{:x}\r\n", data.len());
    let mut out = buf.into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

/// Encode the final (zero-length) chunk.
pub fn encode_last_chunk() -> Vec<u8> {
    b"0\r\n\r\n".to_vec()
}

/// Incremental chunked transfer-encoding decoder.
///
/// Feed raw bytes with `decode()` and collect decoded body data.
pub struct ChunkedDecoder {
    state: ChunkState,
    buf: Vec<u8>,
    output: Vec<u8>,
}

#[derive(Debug, PartialEq)]
enum ChunkState {
    Size,
    Data { remaining: usize },
    DataCrlf,
    FinalCrlf,
    Complete,
}

impl ChunkedDecoder {
    pub fn new() -> Self {
        Self {
            state: ChunkState::Size,
            buf: Vec::new(),
            output: Vec::new(),
        }
    }

    /// Feed bytes and decode. Returns `(consumed, complete)`.
    pub fn decode(&mut self, data: &[u8]) -> Result<(usize, bool), HttpError> {
        let mut pos = 0;
        while pos < data.len() && self.state != ChunkState::Complete {
            match self.state {
                ChunkState::Size => {
                    pos += self.read_size(&data[pos..])?;
                }
                ChunkState::Data { .. } => {
                    pos += self.read_data(&data[pos..]);
                }
                ChunkState::DataCrlf => {
                    pos += self.read_crlf(&data[pos..], ChunkState::Size);
                }
                ChunkState::FinalCrlf => {
                    pos += self.read_crlf(&data[pos..], ChunkState::Complete);
                }
                ChunkState::Complete => break,
            }
        }
        Ok((pos, self.state == ChunkState::Complete))
    }

    /// Take the accumulated decoded body data.
    pub fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Whether decoding is complete.
    pub fn is_complete(&self) -> bool {
        self.state == ChunkState::Complete
    }
}

impl Default for ChunkedDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkedDecoder {
    fn read_size(&mut self, data: &[u8]) -> Result<usize, HttpError> {
        for (i, &b) in data.iter().enumerate() {
            self.buf.push(b);
            if let Some(content) = strip_line_ending(&self.buf) {
                let line =
                    std::str::from_utf8(content).map_err(|_| HttpError::InvalidChunkEncoding)?;
                let size_str = line.split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size_str, 16)
                    .map_err(|_| HttpError::InvalidChunkEncoding)?;

                self.buf.clear();
                if size == 0 {
                    // Read trailing CRLF after the last (zero) chunk
                    self.state = ChunkState::FinalCrlf;
                } else {
                    self.state = ChunkState::Data { remaining: size };
                }
                return Ok(i + 1);
            }
        }
        Ok(data.len())
    }

    fn read_data(&mut self, data: &[u8]) -> usize {
        if let ChunkState::Data { remaining } = &mut self.state {
            let take = data.len().min(*remaining);
            self.output.extend_from_slice(&data[..take]);
            *remaining -= take;
            if *remaining == 0 {
                self.state = ChunkState::DataCrlf;
            }
            take
        } else {
            0
        }
    }

    fn read_crlf(&mut self, data: &[u8], next_state: ChunkState) -> usize {
        for (i, &b) in data.iter().enumerate() {
            self.buf.push(b);
            if strip_line_ending(&self.buf).is_some() {
                self.buf.clear();
                self.state = next_state;
                return i + 1;
            }
        }
        data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_single_chunk() {
        let chunk = encode_chunk(b"hello");
        assert_eq!(chunk, b"5\r\nhello\r\n");
    }

    #[test]
    fn encode_empty_is_last() {
        let chunk = encode_chunk(b"");
        assert_eq!(chunk, b"0\r\n\r\n");
    }

    #[test]
    fn decode_single_chunk() {
        let mut decoder = ChunkedDecoder::new();
        let data = b"5\r\nhello\r\n0\r\n\r\n";
        let (consumed, complete) = decoder.decode(data).unwrap();
        assert_eq!(consumed, data.len());
        assert!(complete);
        assert_eq!(decoder.take_body(), b"hello");
    }

    #[test]
    fn decode_multiple_chunks() {
        let mut decoder = ChunkedDecoder::new();
        let data = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let (_, complete) = decoder.decode(data).unwrap();
        assert!(complete);
        assert_eq!(decoder.take_body(), b"hello world");
    }

    #[test]
    fn roundtrip() {
        let original = b"hello world";
        let mut encoded = encode_chunk(&original[..5]);
        encoded.extend_from_slice(&encode_chunk(&original[5..]));
        encoded.extend_from_slice(&encode_last_chunk());

        let mut decoder = ChunkedDecoder::new();
        let (_, complete) = decoder.decode(&encoded).unwrap();
        assert!(complete);
        assert_eq!(decoder.take_body(), original);
    }
}
