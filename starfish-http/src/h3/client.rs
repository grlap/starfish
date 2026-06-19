//! HTTP/3 client (RFC 9114).
//!
//! [`H3Client`] establishes an HTTP/3 connection to a remote server and offers
//! both a convenience `send()` API and split request/response methods so
//! callers can pipeline multiple streams.

use std::collections::HashMap;
use std::net::SocketAddr;

use starfish_quic::{QuicClient, QuicClientConfig};

use super::connection::{H3Connection, H3Push, LOCAL_MAX_PUSH_ID};
use super::force_h3_alpn;
use crate::content_coding;
use crate::error::HttpError;

/// HTTP/3 request priority expressed using RFC 9218 priority-field syntax.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct H3Priority {
    urgency: u8,
    incremental: bool,
}

impl H3Priority {
    /// Create a priority with urgency `0..=7` and optional incremental delivery.
    pub fn new(urgency: u8, incremental: bool) -> Self {
        Self {
            urgency: urgency.min(7),
            incremental,
        }
    }

    fn as_header_value(self) -> String {
        if self.incremental {
            format!("u={}, i", self.urgency)
        } else {
            format!("u={}", self.urgency)
        }
    }
}

/// HTTP/3 client.
pub struct H3Client {
    conn: H3Connection,
    request_methods: HashMap<u64, http::Method>,
}

impl H3Client {
    /// Connect to a remote HTTP/3 server.
    pub async fn connect(
        addr: SocketAddr,
        server_name: &str,
        mut config: QuicClientConfig,
    ) -> Result<Self, HttpError> {
        force_h3_alpn(&mut config.tls_config.alpn_protocols);
        let quic = QuicClient::connect(addr, server_name, config)
            .await
            .map_err(HttpError::Quic)?;

        let mut conn = H3Connection::new(quic);
        conn.open_control_stream().await?;
        conn.send_max_push_id(LOCAL_MAX_PUSH_ID).await?;

        Ok(Self {
            conn,
            request_methods: HashMap::new(),
        })
    }

    /// Send request headers/body on a new stream and return the stream ID.
    pub async fn send_request(
        &mut self,
        request: http::Request<Vec<u8>>,
    ) -> Result<u64, HttpError> {
        let request_method = request.method().clone();
        let priority = request.extensions().get::<H3Priority>().copied();
        let (header_storage, body) = request_into_h3_parts(&request);
        let header_refs: Vec<(&str, &str)> = header_storage
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();

        let stream_id = self.conn.send_request(&header_refs, body).await?;
        if let Some(priority) = priority {
            let priority_value = priority.as_header_value();
            self.conn
                .send_priority_update(stream_id, priority_value.as_bytes())
                .await?;
        }
        self.request_methods.insert(stream_id, request_method);
        Ok(stream_id)
    }

    /// Read the response for a previously-sent request stream.
    pub async fn recv_response(
        &mut self,
        stream_id: u64,
    ) -> Result<http::Response<Vec<u8>>, HttpError> {
        let (resp_headers, resp_body) = self.conn.read_response(stream_id).await?;
        let request_method = self.request_methods.remove(&stream_id);
        response_from_h3_parts(resp_headers, resp_body, request_method.as_ref())
    }

    /// Send an HTTP/3 request and receive the response.
    pub async fn send(
        &mut self,
        request: http::Request<Vec<u8>>,
    ) -> Result<http::Response<Vec<u8>>, HttpError> {
        let stream_id = self.send_request(request).await?;
        self.recv_response(stream_id).await
    }

    /// Receive the next completed pushed response.
    pub async fn recv_push(&mut self) -> Result<H3Push, HttpError> {
        loop {
            if let Some(push) = self.conn.accept_push() {
                return Ok(push);
            }
            self.conn.poll().await?;
        }
    }

    /// Cancel a server push by push ID.
    pub async fn cancel_push(&mut self, push_id: u64) -> Result<(), HttpError> {
        self.conn.cancel_push(push_id).await
    }

    /// Drive the underlying QUIC/H3 connection.
    pub async fn poll(&mut self) -> Result<(), HttpError> {
        self.conn.poll().await
    }

    /// Get a mutable reference to the underlying H3 connection.
    pub fn connection_mut(&mut self) -> &mut H3Connection {
        &mut self.conn
    }
}

fn request_into_h3_parts(
    request: &http::Request<Vec<u8>>,
) -> (Vec<(String, String)>, Option<&[u8]>) {
    let method = request.method().as_str().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let scheme = request.uri().scheme_str().unwrap_or("https").to_string();
    let authority = request
        .uri()
        .authority()
        .map(|a| a.as_str().to_string())
        .unwrap_or_default();

    let mut headers: Vec<(String, String)> = vec![
        (":method".to_string(), method),
        (":path".to_string(), path),
        (":scheme".to_string(), scheme),
    ];

    if !authority.is_empty() {
        headers.push((":authority".to_string(), authority));
    }

    let mut has_accept_encoding = false;
    let mut has_priority = false;
    for (name, value) in request.headers() {
        if let Ok(v) = value.to_str() {
            if name == http::header::ACCEPT_ENCODING {
                has_accept_encoding = true;
            }
            if name.as_str().eq_ignore_ascii_case("priority") {
                has_priority = true;
            }
            headers.push((name.as_str().to_string(), v.to_string()));
        }
    }

    if !has_accept_encoding {
        headers.push((
            http::header::ACCEPT_ENCODING.as_str().to_string(),
            content_coding::DEFAULT_ACCEPT_ENCODING.to_string(),
        ));
    }
    if !has_priority {
        if let Some(priority) = request.extensions().get::<H3Priority>() {
            headers.push(("priority".to_string(), priority.as_header_value()));
        }
    }

    let body = if request.body().is_empty() {
        None
    } else {
        Some(request.body().as_slice())
    };

    (headers, body)
}

fn h3_message_error(message: impl Into<String>) -> HttpError {
    HttpError::Parse(format!(
        "H3_MESSAGE_ERROR (0x{:04x}): {}",
        crate::h3::error::H3_MESSAGE_ERROR,
        message.into(),
    ))
}

fn validate_response_headers(resp_headers: &[(String, String)]) -> Result<&str, HttpError> {
    let mut status = None;
    let mut saw_regular_header = false;

    for (name, value) in resp_headers {
        if name.starts_with(':') {
            if saw_regular_header {
                return Err(h3_message_error(
                    "response pseudo-header field appeared after a regular header field",
                ));
            }

            if name != ":status" {
                return Err(h3_message_error(format!(
                    "unexpected response pseudo-header {name}",
                )));
            }

            if status.replace(value.as_str()).is_some() {
                return Err(h3_message_error("duplicate :status pseudo-header"));
            }
        } else {
            saw_regular_header = true;
        }
    }

    status.ok_or_else(|| h3_message_error("missing :status pseudo-header"))
}

fn response_from_h3_parts(
    resp_headers: Vec<(String, String)>,
    resp_body: Vec<u8>,
    request_method: Option<&http::Method>,
) -> Result<http::Response<Vec<u8>>, HttpError> {
    let status = validate_response_headers(&resp_headers)?;
    let status: u16 = status
        .parse()
        .map_err(|_| HttpError::Parse("invalid :status".into()))?;
    let status = http::StatusCode::from_u16(status)
        .map_err(|e| HttpError::Parse(format!("invalid status: {e}")))?;

    let mut builder = http::Response::builder().status(status);
    for (name, value) in &resp_headers {
        if !name.starts_with(':') {
            builder = builder.header(name.as_str(), value.as_str());
        }
    }

    let mut response = builder
        .body(resp_body)
        .map_err(|e| HttpError::Parse(format!("build response: {e}")))?;
    content_coding::decode_response_body_for_request(&mut response, request_method)?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::{force_h3_alpn, request_into_h3_parts, response_from_h3_parts, H3Priority};
    use crate::error::HttpError;
    use std::io::Write;

    #[test]
    fn force_h3_alpn_overrides_existing_protocols() {
        let mut protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        force_h3_alpn(&mut protocols);
        assert_eq!(protocols, vec![b"h3".to_vec()]);
    }

    #[test]
    fn response_from_h3_parts_rejects_non_numeric_status() {
        let err = response_from_h3_parts(vec![(":status".into(), "wat".into())], Vec::new(), None)
            .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("invalid :status")));
    }

    #[test]
    fn response_from_h3_parts_rejects_invalid_status_code() {
        let err = response_from_h3_parts(vec![(":status".into(), "99".into())], Vec::new(), None)
            .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("invalid status")));
    }

    #[test]
    fn response_from_h3_parts_rejects_invalid_header_name() {
        let err = response_from_h3_parts(
            vec![
                (":status".into(), "200".into()),
                ("bad header".into(), "value".into()),
            ],
            Vec::new(),
            None,
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("build response")));
    }

    #[test]
    fn response_from_h3_parts_rejects_missing_status() {
        let err = response_from_h3_parts(
            vec![("content-type".into(), "text/plain".into())],
            Vec::new(),
            None,
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_MESSAGE_ERROR")));
    }

    #[test]
    fn response_from_h3_parts_rejects_duplicate_status() {
        let err = response_from_h3_parts(
            vec![
                (":status".into(), "200".into()),
                (":status".into(), "204".into()),
            ],
            Vec::new(),
            None,
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_MESSAGE_ERROR")));
    }

    #[test]
    fn response_from_h3_parts_rejects_unexpected_pseudo_header() {
        let err = response_from_h3_parts(
            vec![
                (":status".into(), "200".into()),
                (":path".into(), "/".into()),
            ],
            Vec::new(),
            None,
        )
        .unwrap_err();

        assert!(matches!(err, HttpError::Parse(message) if message.contains("H3_MESSAGE_ERROR")));
    }

    #[test]
    fn request_into_h3_parts_adds_accept_encoding_and_priority() {
        let mut request = http::Request::builder()
            .method(http::Method::GET)
            .uri("https://example.com/")
            .body(Vec::new())
            .unwrap();
        request.extensions_mut().insert(H3Priority::new(2, true));

        let (headers, _) = request_into_h3_parts(&request);

        assert!(headers
            .iter()
            .any(|(name, value)| name == "accept-encoding" && value == "gzip, br, zstd"));
        assert!(headers
            .iter()
            .any(|(name, value)| name == "priority" && value == "u=2, i"));
    }

    #[test]
    fn response_from_h3_parts_decodes_gzip_body() {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"h3-compressed").unwrap();
        let compressed = encoder.finish().unwrap();

        let response = response_from_h3_parts(
            vec![
                (":status".into(), "200".into()),
                ("content-encoding".into(), "gzip".into()),
            ],
            compressed,
            None,
        )
        .unwrap();

        assert_eq!(response.body().as_slice(), b"h3-compressed");
        assert!(response
            .headers()
            .get(http::header::CONTENT_ENCODING)
            .is_none());
    }

    #[test]
    fn response_from_h3_parts_skips_decode_for_head_requests() {
        let response = response_from_h3_parts(
            vec![
                (":status".into(), "200".into()),
                ("content-encoding".into(), "gzip".into()),
                ("content-length".into(), "24".into()),
            ],
            Vec::new(),
            Some(&http::Method::HEAD),
        )
        .unwrap();

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
