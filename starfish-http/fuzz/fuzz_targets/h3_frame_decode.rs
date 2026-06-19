#![no_main]
//! Fuzzes HTTP/3 frame decoding against arbitrary byte slices.

use libfuzzer_sys::fuzz_target;
use starfish_http::h3::frame::H3Frame;

fuzz_target!(|data: &[u8]| {
    let _ = H3Frame::decode(data);
});
