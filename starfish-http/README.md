# starfish-http

HTTP/1.1 and HTTP/3 for the Starfish runtime.

## Overview

`starfish-http` provides both HTTP/1.1 (RFC 9112) and HTTP/3 (RFC 9114) client and server implementations. HTTP/1.1 operates over TCP (with optional TLS via `starfish-tls`), while HTTP/3 runs over QUIC (`starfish-quic`). All I/O is built on `starfish-reactor`'s `AsyncRead` / `AsyncWrite` traits.

## Modules

| Module | Description |
|--------|-------------|
| `h1::parser` | Incremental HTTP/1.1 request and response parser |
| `h1::encoder` | HTTP/1.1 request and response serialization |
| `h1::chunked` | Chunked transfer-encoding encoder and decoder |
| `h1::connection` | HTTP/1.1 keep-alive connection management |
| `h1::client` | `H1Client` — HTTP/1.1 client |
| `h1::server` | `H1Server` — HTTP/1.1 server |
| `h3::frame` | HTTP/3 frame encoding and decoding |
| `h3::qpack` | QPACK header compression (RFC 9204) |
| `h3::connection` | HTTP/3 connection management over QUIC |
| `h3::client` | `H3Client` — HTTP/3 client |
| `h3::server` | `H3Server` / `H3ServerConnection` — HTTP/3 server |
| `h3::error` | HTTP/3 error codes (RFC 9114 §8) |
| `client` | `HttpClient` — unified client with connection pooling |
| `server` | `HttpServer` — unified TCP server |
| `pool` | Connection pool for HTTP/1.1 keep-alive reuse |
| `error` | `HttpError` — unified error type for I/O, TLS, QUIC, parse, and QPACK failures |

## Key Types & Traits

| Type | Description |
|------|-------------|
| `H1Client<S>` | HTTP/1.1 client, generic over any `AsyncRead + AsyncWrite` stream |
| `H1Server<S>` | HTTP/1.1 server connection handler |
| `H3Client` | HTTP/3 client over QUIC |
| `H3Server` | HTTP/3 server listener |
| `H3ServerConnection` | Per-connection HTTP/3 server state |
| `H3Priority` | RFC 9218 extensible priorities |
| `H3Push` | Server push promise tracking |
| `HttpClient` | Unified HTTP client with auto protocol selection |
| `HttpServer` | Unified TCP HTTP server |
| `HttpError` | Error type covering I/O, TLS, QUIC, parse, and QPACK errors |
| `ConnectionPool` | HTTP/1.1 keep-alive connection pool |
| `ProxyConfig` / `ProxyKind` | Proxy configuration (HTTP CONNECT, SOCKS5) |
| `QpackEncoder` / `QpackDecoder` | QPACK header compression codec |

## Dependencies

```toml
[dependencies]
http = "1.3"
qpack = "0.1"
starfish-reactor = "0.1"
starfish-tls = "0.1"
starfish-quic = "0.1"
```

## Related Crates

| Crate | Description |
|-------|-------------|
| `starfish-reactor` | Async I/O reactor (TCP, UDP) |
| `starfish-tls` | TLS/SSL for HTTPS connections |
| `starfish-quic` | QUIC transport for HTTP/3 |
