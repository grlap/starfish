//! Shared request/response content-coding helpers.

use std::io::{Cursor, Read};

use crate::error::HttpError;

pub(crate) const DEFAULT_ACCEPT_ENCODING: &str = "gzip, br, zstd";

pub(crate) fn ensure_accept_encoding(headers: &mut http::HeaderMap) {
    if headers.contains_key(http::header::ACCEPT_ENCODING) {
        return;
    }

    headers.insert(
        http::header::ACCEPT_ENCODING,
        http::HeaderValue::from_static(DEFAULT_ACCEPT_ENCODING),
    );
}

pub(crate) fn decode_response_body_for_request(
    response: &mut http::Response<Vec<u8>>,
    request_method: Option<&http::Method>,
) -> Result<(), HttpError> {
    if response_must_not_have_body(response, request_method) {
        return Ok(());
    }

    let Some(content_encoding) = response
        .headers()
        .get(http::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(());
    };

    let mut body = response.body().clone();
    for encoding in content_encoding
        .split(',')
        .map(str::trim)
        .filter(|encoding| !encoding.is_empty() && *encoding != "identity")
        .rev()
    {
        body = decode_body(encoding, &body)?;
    }

    *response.body_mut() = body;
    let body_len = response.body().len().to_string();
    response
        .headers_mut()
        .remove(http::header::CONTENT_ENCODING);
    response.headers_mut().remove(http::header::CONTENT_LENGTH);
    response.headers_mut().insert(
        http::header::CONTENT_LENGTH,
        http::HeaderValue::from_str(&body_len)
            .map_err(|e| HttpError::Parse(format!("invalid content length: {e}")))?,
    );
    Ok(())
}

fn response_must_not_have_body(
    response: &http::Response<Vec<u8>>,
    request_method: Option<&http::Method>,
) -> bool {
    if matches!(request_method, Some(method) if *method == http::Method::HEAD) {
        return true;
    }

    response.status().is_informational()
        || response.status() == http::StatusCode::NO_CONTENT
        || response.status() == http::StatusCode::NOT_MODIFIED
}

fn decode_body(encoding: &str, body: &[u8]) -> Result<Vec<u8>, HttpError> {
    match encoding {
        "gzip" => {
            let mut decoder = flate2::read::GzDecoder::new(Cursor::new(body));
            let mut decoded = Vec::new();
            decoder
                .read_to_end(&mut decoded)
                .map_err(|e| HttpError::Parse(format!("gzip decode failed: {e}")))?;
            Ok(decoded)
        }
        "br" => {
            let mut decoder = brotli::Decompressor::new(Cursor::new(body), 4096);
            let mut decoded = Vec::new();
            decoder
                .read_to_end(&mut decoded)
                .map_err(|e| HttpError::Parse(format!("brotli decode failed: {e}")))?;
            Ok(decoded)
        }
        "zstd" => {
            let mut decoder = zstd::stream::read::Decoder::new(Cursor::new(body))
                .map_err(|e| HttpError::Parse(format!("zstd decode failed: {e}")))?;
            let mut decoded = Vec::new();
            decoder
                .read_to_end(&mut decoded)
                .map_err(|e| HttpError::Parse(format!("zstd decode failed: {e}")))?;
            Ok(decoded)
        }
        other => Err(HttpError::Parse(format!(
            "unsupported content-encoding: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn response_with_encoding(encoding: &str, body: Vec<u8>) -> http::Response<Vec<u8>> {
        http::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::CONTENT_ENCODING, encoding)
            .header(http::header::CONTENT_LENGTH, body.len().to_string())
            .body(body)
            .unwrap()
    }

    #[test]
    fn ensure_accept_encoding_is_idempotent() {
        let mut headers = http::HeaderMap::new();
        ensure_accept_encoding(&mut headers);
        ensure_accept_encoding(&mut headers);

        assert_eq!(
            headers
                .get(http::header::ACCEPT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some(DEFAULT_ACCEPT_ENCODING)
        );
    }

    #[test]
    fn decode_gzip_response_body() {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"hello gzip").unwrap();
        let compressed = encoder.finish().unwrap();

        let mut response = response_with_encoding("gzip", compressed);
        decode_response_body_for_request(&mut response, None).unwrap();

        assert_eq!(response.body(), b"hello gzip");
        assert!(response
            .headers()
            .get(http::header::CONTENT_ENCODING)
            .is_none());
    }

    #[test]
    fn decode_brotli_response_body() {
        let mut compressed = Vec::new();
        {
            let mut compressor = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            compressor.write_all(b"hello brotli").unwrap();
        }

        let mut response = response_with_encoding("br", compressed);
        decode_response_body_for_request(&mut response, None).unwrap();

        assert_eq!(response.body(), b"hello brotli");
    }

    #[test]
    fn decode_zstd_response_body() {
        let compressed = zstd::stream::encode_all(Cursor::new(b"hello zstd"), 0).unwrap();
        let mut response = response_with_encoding("zstd", compressed);
        decode_response_body_for_request(&mut response, None).unwrap();

        assert_eq!(response.body(), b"hello zstd");
    }

    #[test]
    fn decode_skips_head_responses_without_body() {
        let mut response = response_with_encoding("gzip", Vec::new());
        decode_response_body_for_request(&mut response, Some(&http::Method::HEAD)).unwrap();

        assert!(response.body().is_empty());
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
    }

    #[test]
    fn decode_skips_not_modified_responses_without_body() {
        let mut response = http::Response::builder()
            .status(http::StatusCode::NOT_MODIFIED)
            .header(http::header::CONTENT_ENCODING, "gzip")
            .body(Vec::new())
            .unwrap();
        decode_response_body_for_request(&mut response, None).unwrap();

        assert!(response.body().is_empty());
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
    }
}
