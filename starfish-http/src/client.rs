//! Unified HTTP client with redirect following, TLS, cookies, and proxying.
//!
//! [`HttpClient`] routes HTTP/1.1 requests over plain TCP or TLS, follows
//! redirects, reuses pooled connections, stores cookies, and supports HTTP and
//! SOCKS5 proxies.

use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::time::{Duration, Instant};

use rustls::{ClientConfig, RootCertStore};
use starfish_reactor::cooperative_io::async_read::{AsyncRead, AsyncReadExtension};
use starfish_reactor::cooperative_io::async_write::{AsyncWrite, AsyncWriteExtension};
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;
use starfish_reactor::cooperative_io::tcp_stream::TcpStream;
use starfish_tls::tls_client::TlsClient;

use crate::error::HttpError;
use crate::h1::client::H1Client;
use crate::h1::connection::H1Connection;
use crate::h1::parser::ResponseParser;
use crate::pool::{ConnectionPool, PoolKey};

/// Default maximum idle time for pooled connections (60 seconds).
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Default maximum number of redirects to follow.
const DEFAULT_MAX_REDIRECTS: u32 = 10;

type PooledConnection = H1Connection<ClientTransport>;

/// Supported outbound proxy schemes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyKind {
    /// HTTP proxy. Plain HTTP requests use absolute-form URIs; HTTPS uses CONNECT.
    Http,
    /// SOCKS5 proxy without authentication.
    Socks5,
}

/// Proxy configuration for [`HttpClient`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyConfig {
    kind: ProxyKind,
    host: String,
    port: u16,
}

impl ProxyConfig {
    /// Parse a proxy URI such as `http://127.0.0.1:8080` or `socks5://127.0.0.1:1080`.
    pub fn parse(proxy_uri: &str) -> Result<Self, HttpError> {
        let uri: http::Uri = proxy_uri
            .parse()
            .map_err(|e| HttpError::Parse(format!("invalid proxy URI: {e}")))?;
        let host = uri
            .host()
            .ok_or_else(|| HttpError::Parse("proxy URI is missing a host".into()))?;
        let kind = match uri.scheme_str() {
            Some("http") => ProxyKind::Http,
            Some("socks5") | Some("socks5h") => ProxyKind::Socks5,
            Some(other) => {
                return Err(HttpError::Parse(format!(
                    "unsupported proxy scheme: {other}"
                )));
            }
            None => return Err(HttpError::Parse("proxy URI is missing a scheme".into())),
        };
        let port = uri.port_u16().unwrap_or(match kind {
            ProxyKind::Http => 8080,
            ProxyKind::Socks5 => 1080,
        });
        Ok(Self {
            kind,
            host: host.to_string(),
            port,
        })
    }

    fn pool_key(&self) -> String {
        let scheme = match self.kind {
            ProxyKind::Http => "http",
            ProxyKind::Socks5 => "socks5",
        };
        format!("{scheme}://{}:{}", self.host, self.port)
    }
}

enum ClientTransport {
    Tcp(TcpStream),
    Tls(Box<TlsClient<TcpStream>>),
}

impl AsyncRead for ClientTransport {
    async fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read_with_timeout(buf, timeout).await,
            Self::Tls(stream) => stream.read_with_timeout(buf, timeout).await,
        }
    }
}

impl AsyncWrite for ClientTransport {
    async fn write_with_timeout(
        &mut self,
        buf: &[u8],
        timeout: &Option<IOTimeout>,
    ) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write_with_timeout(buf, timeout).await,
            Self::Tls(stream) => stream.write_with_timeout(buf, timeout).await,
        }
    }

    async fn flush_with_timeout(&mut self, timeout: &Option<IOTimeout>) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush_with_timeout(timeout).await,
            Self::Tls(stream) => stream.flush_with_timeout(timeout).await,
        }
    }

    async fn close(&mut self) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.close().await,
            Self::Tls(stream) => stream.close().await,
        }
    }
}

#[derive(Default)]
struct CookieJar {
    cookies: Vec<StoredCookie>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    secure: bool,
    host_only: bool,
    expires_at: Option<Instant>,
}

enum CookieStoreOp {
    Put(StoredCookie),
    Delete {
        name: String,
        domain: String,
        path: String,
    },
}

impl CookieJar {
    fn store(&mut self, uri: &http::Uri, headers: &http::HeaderMap) {
        let Some(host) = uri.host() else {
            return;
        };
        let default_path = default_cookie_path(uri);
        let secure_origin = uri.scheme_str() == Some("https");

        for header_value in headers.get_all(http::header::SET_COOKIE) {
            let Ok(raw) = header_value.to_str() else {
                continue;
            };
            let Some(op) = parse_set_cookie(raw, host, &default_path, secure_origin) else {
                continue;
            };

            match op {
                CookieStoreOp::Put(cookie) => self.upsert(cookie),
                CookieStoreOp::Delete { name, domain, path } => {
                    self.cookies.retain(|cookie| {
                        !(cookie.name == name && cookie.domain == domain && cookie.path == path)
                    });
                }
            }
        }

        self.remove_expired();
    }

    fn apply(&mut self, uri: &http::Uri, headers: &mut http::HeaderMap) {
        if headers.contains_key(http::header::COOKIE) {
            return;
        }

        let Some(host) = uri.host() else {
            return;
        };
        self.remove_expired();

        let request_path = uri.path();
        let secure = uri.scheme_str() == Some("https");
        let mut matching: Vec<&StoredCookie> = self
            .cookies
            .iter()
            .filter(|cookie| cookie.matches(host, request_path, secure))
            .collect();
        matching.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));

        if matching.is_empty() {
            return;
        }

        let cookie_header = matching
            .into_iter()
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; ");
        if let Ok(value) = http::HeaderValue::from_str(&cookie_header) {
            headers.insert(http::header::COOKIE, value);
        }
    }

    fn upsert(&mut self, cookie: StoredCookie) {
        self.cookies.retain(|existing| {
            !(existing.name == cookie.name
                && existing.domain == cookie.domain
                && existing.path == cookie.path)
        });
        self.cookies.push(cookie);
    }

    fn remove_expired(&mut self) {
        let now = Instant::now();
        self.cookies.retain(|cookie| {
            cookie
                .expires_at
                .map(|expires_at| expires_at > now)
                .unwrap_or(true)
        });
    }
}

impl StoredCookie {
    fn matches(&self, host: &str, request_path: &str, secure: bool) -> bool {
        if self.secure && !secure {
            return false;
        }
        if self.host_only {
            if !host.eq_ignore_ascii_case(&self.domain) {
                return false;
            }
        } else if !domain_matches(host, &self.domain) {
            return false;
        }

        path_matches(request_path, &self.path)
    }
}

/// Unified HTTP client supporting HTTP/1.1 over TCP or TLS.
pub struct HttpClient {
    pool: ConnectionPool<PooledConnection>,
    max_redirects: u32,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    tls_config: ClientConfig,
    cookie_jar: CookieJar,
    proxy: Option<ProxyConfig>,
}

impl HttpClient {
    /// Create a new HttpClient with default settings.
    pub fn new() -> Self {
        Self {
            pool: ConnectionPool::new(8),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            read_timeout: None,
            write_timeout: None,
            tls_config: default_tls_config(),
            cookie_jar: CookieJar::default(),
            proxy: None,
        }
    }

    /// Replace the TLS configuration used for `https://` requests.
    pub fn set_tls_config(&mut self, tls_config: ClientConfig) {
        self.tls_config = tls_config;
        self.pool.clear();
    }

    /// Configure an outbound proxy or clear proxying with `None`.
    pub fn set_proxy(&mut self, proxy: Option<ProxyConfig>) {
        self.proxy = proxy;
        self.pool.clear();
    }

    /// Set the read timeout for response reads.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    /// Set the write timeout for request writes.
    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    /// Set the maximum number of redirects to follow. Default is 10.
    pub fn set_max_redirects(&mut self, max: u32) {
        self.max_redirects = max;
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    /// Send an HTTP request, automatically following redirects.
    ///
    /// Follows 301/302/303/307/308 redirects up to `max_redirects` hops.
    /// For 301/302/303, non-GET/HEAD methods are changed to GET and the body
    /// is dropped. For 307/308, the method and body are preserved.
    /// Sensitive headers (`Authorization`, `Cookie`) are stripped on
    /// cross-origin redirects.
    pub async fn send(
        &mut self,
        request: http::Request<Vec<u8>>,
    ) -> Result<http::Response<Vec<u8>>, HttpError> {
        let (parts, original_body) = request.into_parts();
        let original_uri = parts.uri.clone();
        let original_headers = parts.headers;
        let mut method = parts.method;
        let mut uri = parts.uri;
        let mut body = original_body;
        let mut redirects_remaining = self.max_redirects;

        loop {
            let req =
                build_redirect_request(&method, &uri, &original_headers, &original_uri, &body)?;
            let response = self.send_single(req).await?;

            if !response.status().is_redirection() || redirects_remaining == 0 {
                return Ok(response);
            }

            let location = match response.headers().get(http::header::LOCATION) {
                Some(loc) => loc
                    .to_str()
                    .map_err(|_| HttpError::Parse("invalid Location header".into()))?,
                None => return Ok(response),
            };

            let new_uri = resolve_redirect_uri(&uri, location)?;

            match response.status().as_u16() {
                301..=303 => {
                    if method != http::Method::GET && method != http::Method::HEAD {
                        method = http::Method::GET;
                        body = Vec::new();
                    }
                }
                307 | 308 => {}
                _ => return Ok(response),
            }

            uri = new_uri;
            redirects_remaining -= 1;
        }
    }

    async fn send_single(
        &mut self,
        request: http::Request<Vec<u8>>,
    ) -> Result<http::Response<Vec<u8>>, HttpError> {
        let uri = request.uri().clone();
        let host = uri
            .host()
            .ok_or_else(|| HttpError::Parse("missing host in URI".into()))?;
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            });
        let is_tls = uri.scheme_str() == Some("https");
        let proxy = self.proxy.clone();
        let tls_config = self.tls_config.clone();

        let key = PoolKey {
            host: host.to_string(),
            port,
            is_tls,
            proxy: proxy.as_ref().map(ProxyConfig::pool_key),
        };

        let conn = loop {
            if let Some(conn) = self.pool.take(&key) {
                if conn.is_likely_healthy(POOL_IDLE_TIMEOUT) {
                    break conn;
                }
                continue;
            }

            let stream = connect_transport(
                host,
                port,
                is_tls,
                proxy.as_ref(),
                &tls_config,
                &self.read_timeout,
                &self.write_timeout,
            )
            .await?;
            break H1Connection::new(stream);
        };

        let mut client = H1Client::from_connection(conn);
        if let Some(timeout) = self.read_timeout {
            client.set_read_timeout(Some(timeout));
        }
        if let Some(timeout) = self.write_timeout {
            client.set_write_timeout(Some(timeout));
        }

        let mut request = prepare_request_for_transport(request, proxy.as_ref(), is_tls)?;
        self.cookie_jar.apply(&uri, request.headers_mut());
        let response = client.send(request).await?;
        self.cookie_jar.store(&uri, response.headers());

        if client.is_reusable() {
            self.pool.put(key, client.into_connection());
        }

        Ok(response)
    }
}

fn default_tls_config() -> ClientConfig {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let root_store = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    let mut config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}

async fn connect_transport(
    host: &str,
    port: u16,
    is_tls: bool,
    proxy: Option<&ProxyConfig>,
    tls_config: &ClientConfig,
    read_timeout: &Option<Duration>,
    write_timeout: &Option<Duration>,
) -> Result<ClientTransport, HttpError> {
    let proxy_timeout = pick_timeout(read_timeout, write_timeout);
    let io_timeout = proxy_timeout.map(IOTimeout::from_duration);

    match proxy {
        Some(proxy) => {
            let mut stream = connect_tcp(&proxy.host, proxy.port).await?;

            match proxy.kind {
                ProxyKind::Http => {
                    if is_tls {
                        establish_http_tunnel(&mut stream, host, port, &io_timeout).await?;
                        let tls = TlsClient::new(stream, tls_config.clone(), host.to_string())
                            .await
                            .map_err(HttpError::Tls)?;
                        Ok(ClientTransport::Tls(Box::new(tls)))
                    } else {
                        Ok(ClientTransport::Tcp(stream))
                    }
                }
                ProxyKind::Socks5 => {
                    establish_socks5_tunnel(&mut stream, host, port, &io_timeout).await?;
                    if is_tls {
                        let tls = TlsClient::new(stream, tls_config.clone(), host.to_string())
                            .await
                            .map_err(HttpError::Tls)?;
                        Ok(ClientTransport::Tls(Box::new(tls)))
                    } else {
                        Ok(ClientTransport::Tcp(stream))
                    }
                }
            }
        }
        None => {
            let stream = connect_tcp(host, port).await?;
            if is_tls {
                let tls = TlsClient::new(stream, tls_config.clone(), host.to_string())
                    .await
                    .map_err(HttpError::Tls)?;
                Ok(ClientTransport::Tls(Box::new(tls)))
            } else {
                Ok(ClientTransport::Tcp(stream))
            }
        }
    }
}

fn pick_timeout(
    read_timeout: &Option<Duration>,
    write_timeout: &Option<Duration>,
) -> Option<Duration> {
    match (read_timeout, write_timeout) {
        (Some(read), Some(write)) => Some((*read).min(*write)),
        (Some(read), None) => Some(*read),
        (None, Some(write)) => Some(*write),
        (None, None) => None,
    }
}

fn resolve_socket_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, HttpError> {
    let addrs = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|e| HttpError::Parse(format!("failed to resolve {host}:{port}: {e}")))?
        .collect::<Vec<_>>();

    if addrs.is_empty() {
        return Err(HttpError::Parse(format!(
            "no addresses found for {host}:{port}"
        )));
    }

    Ok(addrs)
}

async fn connect_tcp(host: &str, port: u16) -> Result<TcpStream, HttpError> {
    let addrs = resolve_socket_addrs(host, port)?;
    let mut last_err = None;

    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(err) => last_err = Some((addr, err)),
        }
    }

    let (addr, err) = last_err.expect("resolved address list was non-empty");
    Err(HttpError::Io(io::Error::new(
        err.kind(),
        format!("failed to connect to {host}:{port} via {addr}: {err}"),
    )))
}

async fn establish_http_tunnel<S: AsyncRead + AsyncWrite>(
    stream: &mut S,
    host: &str,
    port: u16,
    timeout: &Option<IOTimeout>,
) -> Result<(), HttpError> {
    let authority = format_authority(host, port);
    let request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
    );
    stream
        .write_all_with_timeout(request.as_bytes(), timeout)
        .await
        .map_err(map_timeout_error)?;
    stream
        .flush_with_timeout(timeout)
        .await
        .map_err(map_timeout_error)?;

    let mut parser = ResponseParser::new();
    let mut read_buf = [0u8; 1];
    loop {
        let n = stream
            .read_with_timeout(&mut read_buf, timeout)
            .await
            .map_err(map_timeout_error)?;
        if n == 0 {
            return Err(HttpError::UnexpectedEof);
        }
        parser.feed(&read_buf[..n])?;
        if !parser.is_head_complete() {
            continue;
        }

        parser.complete_without_body();
        let response = parser.take_response()?;
        if !response.status().is_success() {
            return Err(HttpError::Parse(format!(
                "proxy CONNECT failed with status {}",
                response.status()
            )));
        }
        return Ok(());
    }
}

async fn establish_socks5_tunnel(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    timeout: &Option<IOTimeout>,
) -> Result<(), HttpError> {
    stream
        .write_all_with_timeout(&[0x05, 0x01, 0x00], timeout)
        .await
        .map_err(map_timeout_error)?;
    stream
        .flush_with_timeout(timeout)
        .await
        .map_err(map_timeout_error)?;

    let mut greeting = [0u8; 2];
    stream
        .read_exact_with_timeout(&mut greeting, timeout)
        .await
        .map_err(map_timeout_error)?;
    if greeting != [0x05, 0x00] {
        return Err(HttpError::Parse(
            "SOCKS5 proxy rejected no-auth handshake".into(),
        ));
    }

    let mut request = vec![0x05, 0x01, 0x00];
    if let Ok(ip) = IpAddr::from_str(host) {
        match ip {
            IpAddr::V4(ip) => {
                request.push(0x01);
                request.extend_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                request.push(0x04);
                request.extend_from_slice(&ip.octets());
            }
        }
    } else {
        let host_bytes = host.as_bytes();
        if host_bytes.len() > u8::MAX as usize {
            return Err(HttpError::Parse("SOCKS5 host name is too long".into()));
        }
        request.push(0x03);
        request.push(host_bytes.len() as u8);
        request.extend_from_slice(host_bytes);
    }
    request.extend_from_slice(&port.to_be_bytes());

    stream
        .write_all_with_timeout(&request, timeout)
        .await
        .map_err(map_timeout_error)?;
    stream
        .flush_with_timeout(timeout)
        .await
        .map_err(map_timeout_error)?;

    let mut reply_head = [0u8; 4];
    stream
        .read_exact_with_timeout(&mut reply_head, timeout)
        .await
        .map_err(map_timeout_error)?;
    if reply_head[0] != 0x05 {
        return Err(HttpError::Parse(
            "invalid SOCKS5 proxy response version".into(),
        ));
    }
    if reply_head[1] != 0x00 {
        return Err(HttpError::Parse(format!(
            "SOCKS5 proxy connect failed with code 0x{:02x}",
            reply_head[1]
        )));
    }

    let addr_len = match reply_head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            stream
                .read_exact_with_timeout(&mut len, timeout)
                .await
                .map_err(map_timeout_error)?;
            len[0] as usize
        }
        _ => return Err(HttpError::Parse("invalid SOCKS5 address type".into())),
    };

    let mut discard = vec![0u8; addr_len + 2];
    stream
        .read_exact_with_timeout(&mut discard, timeout)
        .await
        .map_err(map_timeout_error)?;
    Ok(())
}

fn format_authority(host: &str, port: u16) -> String {
    match IpAddr::from_str(host) {
        Ok(IpAddr::V6(_)) => format!("[{host}]:{port}"),
        _ => format!("{host}:{port}"),
    }
}

fn map_timeout_error(error: std::io::Error) -> HttpError {
    if error.kind() == std::io::ErrorKind::TimedOut {
        HttpError::Timeout
    } else {
        HttpError::Io(error)
    }
}

fn prepare_request_for_transport(
    mut request: http::Request<Vec<u8>>,
    proxy: Option<&ProxyConfig>,
    is_tls: bool,
) -> Result<http::Request<Vec<u8>>, HttpError> {
    let use_absolute_form =
        matches!(proxy.map(|proxy| proxy.kind), Some(ProxyKind::Http)) && !is_tls;
    if use_absolute_form {
        return Ok(request);
    }

    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    *request.uri_mut() = path
        .parse()
        .map_err(|e| HttpError::Parse(format!("invalid request path: {e}")))?;
    Ok(request)
}

fn parse_set_cookie(
    raw: &str,
    response_host: &str,
    default_path: &str,
    secure_origin: bool,
) -> Option<CookieStoreOp> {
    let mut segments = raw.split(';').map(str::trim);
    let first = segments.next()?;
    let (name, value) = first.split_once('=')?;
    if name.is_empty() {
        return None;
    }

    let mut domain = response_host.to_ascii_lowercase();
    let mut path = default_path.to_string();
    let mut host_only = true;
    let mut secure = false;
    let mut max_age = None;

    for attr in segments {
        let (attr_name, attr_value) = match attr.split_once('=') {
            Some((name, value)) => (name.trim().to_ascii_lowercase(), Some(value.trim())),
            None => (attr.to_ascii_lowercase(), None),
        };

        match attr_name.as_str() {
            "domain" => {
                let value = attr_value?;
                let normalized = value.trim_start_matches('.').to_ascii_lowercase();
                if normalized.is_empty() || !domain_matches(response_host, &normalized) {
                    return None;
                }
                domain = normalized;
                host_only = false;
            }
            "path" => {
                if let Some(value) = attr_value {
                    path = if value.starts_with('/') {
                        value.to_string()
                    } else {
                        default_path.to_string()
                    };
                }
            }
            "secure" => {
                if !secure_origin {
                    return None;
                }
                secure = true;
            }
            "max-age" => {
                max_age = attr_value.and_then(|value| value.parse::<i64>().ok());
            }
            _ => {}
        }
    }

    let expires_at = match max_age {
        Some(age) if age <= 0 => {
            return Some(CookieStoreOp::Delete {
                name: name.to_string(),
                domain,
                path,
            });
        }
        Some(age) => Some(Instant::now() + Duration::from_secs(age as u64)),
        None => None,
    };

    Some(CookieStoreOp::Put(StoredCookie {
        name: name.to_string(),
        value: value.to_string(),
        domain,
        path,
        secure,
        host_only,
        expires_at,
    }))
}

fn default_cookie_path(uri: &http::Uri) -> String {
    let path = uri.path();
    if !path.starts_with('/') || path == "/" {
        return "/".into();
    }
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(index) => path[..index].to_string(),
    }
}

fn domain_matches(host: &str, domain: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')
}

/// Build a request for the current redirect hop.
///
/// Copies original headers, updates the Host header for the target URI,
/// and strips sensitive headers on cross-origin redirects.
fn build_redirect_request(
    method: &http::Method,
    uri: &http::Uri,
    original_headers: &http::HeaderMap,
    original_uri: &http::Uri,
    body: &[u8],
) -> Result<http::Request<Vec<u8>>, HttpError> {
    let mut builder = http::Request::builder()
        .method(method.clone())
        .uri(uri.clone());

    let same_origin = is_same_origin(original_uri, uri);
    for (name, value) in original_headers {
        if name == http::header::HOST {
            continue;
        }
        if !same_origin && (name == http::header::AUTHORIZATION || name == http::header::COOKIE) {
            continue;
        }
        builder = builder.header(name, value);
    }

    if let Some(authority) = uri.authority() {
        builder = builder.header(http::header::HOST, authority.as_str());
    }

    builder
        .body(body.to_vec())
        .map_err(|e| HttpError::Parse(format!("build request: {e}")))
}

/// Resolve a redirect `Location` URI against the current request URI.
///
/// Handles absolute URIs, protocol-relative URIs, absolute paths, and
/// relative paths.
fn resolve_redirect_uri(base: &http::Uri, location: &str) -> Result<http::Uri, HttpError> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location
            .parse()
            .map_err(|e| HttpError::Parse(format!("invalid redirect URI: {e}")));
    }

    if location.starts_with("//") {
        let scheme = base.scheme_str().unwrap_or("http");
        return format!("{scheme}:{location}")
            .parse()
            .map_err(|e| HttpError::Parse(format!("invalid redirect URI: {e}")));
    }

    let scheme = base.scheme_str().unwrap_or("http");
    let authority = base.authority().map(|a| a.as_str()).unwrap_or("");

    let path = if location.starts_with('/') {
        location.to_string()
    } else {
        let base_path = base.path();
        let base_dir = match base_path.rfind('/') {
            Some(index) => &base_path[..=index],
            None => "/",
        };
        format!("{base_dir}{location}")
    };

    format!("{scheme}://{authority}{path}")
        .parse()
        .map_err(|e| HttpError::Parse(format!("invalid redirect URI: {e}")))
}

/// Check if two URIs have the same origin (scheme + host + port).
fn is_same_origin(a: &http::Uri, b: &http::Uri) -> bool {
    a.scheme_str() == b.scheme_str() && a.host() == b.host() && a.port_u16() == b.port_u16()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::io;
    use std::rc::Rc;

    use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
    use starfish_reactor::cooperative_io::io_timeout::IOTimeout;

    #[derive(Default)]
    struct MockState {
        reads: VecDeque<io::Result<Vec<u8>>>,
        written: Vec<u8>,
        flushes: usize,
    }

    #[derive(Clone, Default)]
    struct MockStream {
        state: Rc<RefCell<MockState>>,
    }

    impl MockStream {
        fn with_reads(reads: Vec<io::Result<Vec<u8>>>) -> Self {
            Self {
                state: Rc::new(RefCell::new(MockState {
                    reads: VecDeque::from(reads),
                    written: Vec::new(),
                    flushes: 0,
                })),
            }
        }

        fn written_bytes(&self) -> Vec<u8> {
            self.state.borrow().written.clone()
        }

        fn flush_count(&self) -> usize {
            self.state.borrow().flushes
        }
    }

    impl AsyncRead for MockStream {
        fn read_with_timeout(
            &mut self,
            buf: &mut [u8],
            _timeout: &Option<IOTimeout>,
        ) -> impl Future<Output = io::Result<usize>> {
            let state = self.state.clone();

            async move {
                let next = state
                    .borrow_mut()
                    .reads
                    .pop_front()
                    .unwrap_or_else(|| Ok(Vec::new()));
                let mut chunk = next?;
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                if n < chunk.len() {
                    chunk.drain(..n);
                    state.borrow_mut().reads.push_front(Ok(chunk));
                }
                Ok(n)
            }
        }
    }

    impl AsyncWrite for MockStream {
        fn write_with_timeout(
            &mut self,
            buf: &[u8],
            _timeout: &Option<IOTimeout>,
        ) -> impl Future<Output = io::Result<usize>> {
            let state = self.state.clone();
            let data = buf.to_vec();
            async move {
                state.borrow_mut().written.extend_from_slice(&data);
                Ok(data.len())
            }
        }

        fn flush_with_timeout(
            &mut self,
            _timeout: &Option<IOTimeout>,
        ) -> impl Future<Output = io::Result<()>> {
            let state = self.state.clone();
            async move {
                state.borrow_mut().flushes += 1;
                Ok(())
            }
        }
    }

    #[test]
    fn resolve_absolute_uri() {
        let base: http::Uri = "http://example.com/old".parse().unwrap();
        let result = resolve_redirect_uri(&base, "http://other.com/new").unwrap();
        assert_eq!(result, "http://other.com/new");
    }

    #[test]
    fn resolve_absolute_path() {
        let base: http::Uri = "http://example.com/old/page".parse().unwrap();
        let result = resolve_redirect_uri(&base, "/new/page").unwrap();
        assert_eq!(result, "http://example.com/new/page");
    }

    #[test]
    fn resolve_relative_path() {
        let base: http::Uri = "http://example.com/dir/page".parse().unwrap();
        let result = resolve_redirect_uri(&base, "other").unwrap();
        assert_eq!(result, "http://example.com/dir/other");
    }

    #[test]
    fn resolve_protocol_relative() {
        let base: http::Uri = "https://example.com/page".parse().unwrap();
        let result = resolve_redirect_uri(&base, "//other.com/new").unwrap();
        assert_eq!(result, "https://other.com/new");
    }

    #[test]
    fn same_origin_check() {
        let a: http::Uri = "http://example.com/a".parse().unwrap();
        let b: http::Uri = "http://example.com/b".parse().unwrap();
        let c: http::Uri = "https://example.com/a".parse().unwrap();
        let d: http::Uri = "http://other.com/a".parse().unwrap();

        assert!(is_same_origin(&a, &b));
        assert!(!is_same_origin(&a, &c));
        assert!(!is_same_origin(&a, &d));
    }

    #[test]
    fn default_client_settings() {
        let client = HttpClient::new();
        assert_eq!(client.max_redirects, DEFAULT_MAX_REDIRECTS);
        assert!(client.read_timeout.is_none());
        assert!(client.write_timeout.is_none());
        assert!(client.proxy.is_none());
    }

    #[test]
    fn client_set_timeouts() {
        let mut client = HttpClient::new();
        client.set_read_timeout(Some(Duration::from_secs(30)));
        client.set_write_timeout(Some(Duration::from_secs(10)));
        client.set_max_redirects(5);
        assert_eq!(client.read_timeout, Some(Duration::from_secs(30)));
        assert_eq!(client.write_timeout, Some(Duration::from_secs(10)));
        assert_eq!(client.max_redirects, 5);
    }

    #[test]
    fn proxy_config_parses_supported_schemes() {
        let http_proxy = ProxyConfig::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!(http_proxy.kind, ProxyKind::Http);
        assert_eq!(http_proxy.host, "127.0.0.1");
        assert_eq!(http_proxy.port, 8080);

        let socks_proxy = ProxyConfig::parse("socks5://localhost:1080").unwrap();
        assert_eq!(socks_proxy.kind, ProxyKind::Socks5);
        assert_eq!(socks_proxy.host, "localhost");
        assert_eq!(socks_proxy.port, 1080);
    }

    #[test]
    fn prepare_request_uses_origin_form_without_plain_http_proxy() {
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri("https://example.com/a/b?c=1")
            .body(Vec::new())
            .unwrap();

        let request = prepare_request_for_transport(request, None, true).unwrap();
        assert_eq!(request.uri(), &"/a/b?c=1");
    }

    #[test]
    fn prepare_request_preserves_absolute_form_for_http_proxy() {
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri("http://example.com/a/b?c=1")
            .body(Vec::new())
            .unwrap();

        let proxy = ProxyConfig::parse("http://127.0.0.1:8080").unwrap();
        let request = prepare_request_for_transport(request, Some(&proxy), false).unwrap();
        assert_eq!(request.uri(), &"http://example.com/a/b?c=1");
    }

    #[test]
    fn cookie_jar_applies_matching_cookie_only() {
        let mut jar = CookieJar::default();
        let response_uri: http::Uri = "https://example.com/app/index".parse().unwrap();
        let mut headers = http::HeaderMap::new();
        headers.append(
            http::header::SET_COOKIE,
            http::HeaderValue::from_static("session=abc; Path=/app; Secure"),
        );
        headers.append(
            http::header::SET_COOKIE,
            http::HeaderValue::from_static("theme=dark; Path=/other"),
        );

        jar.store(&response_uri, &headers);

        let mut request_headers = http::HeaderMap::new();
        let request_uri: http::Uri = "https://example.com/app/dashboard".parse().unwrap();
        jar.apply(&request_uri, &mut request_headers);

        assert_eq!(
            request_headers
                .get(http::header::COOKIE)
                .and_then(|value| value.to_str().ok()),
            Some("session=abc")
        );
    }

    #[test]
    fn cookie_parser_rejects_foreign_domain() {
        let result = parse_set_cookie("session=abc; Domain=other.test", "example.com", "/", false);
        assert!(result.is_none());
    }

    #[test]
    fn http_connect_tunnel_accepts_header_only_success_response() {
        let stream = MockStream::with_reads(vec![
            Ok(b"HTTP/1.1 200 Connection Established\r\n".to_vec()),
            Ok(b"Proxy-Agent: test\r\n\r\n".to_vec()),
        ]);
        let mut stream = stream.clone();

        establish_http_tunnel(&mut stream, "example.test", 443, &None)
            .unwrap_result()
            .unwrap();

        assert_eq!(stream.flush_count(), 1);
        let written = String::from_utf8(stream.written_bytes()).unwrap();
        assert_eq!(
            written,
            "CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\nProxy-Connection: Keep-Alive\r\n\r\n"
        );
    }
}
