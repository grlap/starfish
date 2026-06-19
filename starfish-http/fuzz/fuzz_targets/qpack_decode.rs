#![no_main]
//! Fuzzes QPACK header-block decoding against arbitrary byte slices.

use libfuzzer_sys::fuzz_target;
use starfish_http::h3::qpack::decoder::QpackDecoder;

fuzz_target!(|data: &[u8]| {
    let decoder = QpackDecoder::new();
    let _ = decoder.decode_header_block(data);
});
