# starfish-quic

QUIC transport protocol implementation (RFC 9000) for the Starfish runtime.

## Overview

`starfish-quic` provides a from-scratch QUIC implementation built on `starfish-reactor`'s `UdpSocket` and `rustls` for TLS 1.3 integration (RFC 9001). Includes RFC 9002-compliant loss detection and congestion control.

## Modules

| Module | Description |
|--------|-------------|
| `packet` | Wire-format packet parsing and serialization (RFC 9000 §17) |
| `frame` | QUIC frame encoding and decoding (RFC 9000 §19) |
| `crypto` | Packet protection: AEAD, header protection, key derivation (RFC 9001) |
| `transport` | Connection IDs, flow control, stream state machines |
| `recovery` | Loss detection and congestion control — NewReno + Cubic (RFC 9002) |
| `connection` | `QuicConnection` — main connection type |
| `stream` | `QuicStream` — `AsyncRead + AsyncWrite` over a QUIC stream |
| `client` | `QuicClient` — initiates QUIC connections |
| `server` | `QuicListener` + `QuicEndpoint` — single- and multi-connection servers |
| `error` | `QuicError` and `TransportErrorCode` — typed error handling |

## Key Types & Traits

| Type | Description |
|------|-------------|
| `QuicConnection` | Full QUIC connection: handshake, packet I/O, stream management |
| `QuicStream` | Single QUIC stream implementing `AsyncRead + AsyncWrite` |
| `QuicClientConfig` | Client configuration (TLS, CID length) |
| `QuicClient` | Client-side connection establishment |
| `QuicListener` | Server-side single-connection acceptance |
| `QuicEndpoint` | Multi-connection server with DCID-based datagram routing |
| `QuicServerConfig` | Server configuration (TLS, CID length, Retry) |
| `QuicError` | Unified error type covering I/O, TLS, and transport errors |
| `CongestionController` | Trait for pluggable congestion control algorithms |

## Dependencies

```toml
[dependencies]
ring = "0.17"
rustls = { version = "0.23", features = ["ring"] }
starfish-reactor = "0.1"
rand = "0.9"
```

## Related Crates

| Crate | Description |
|-------|-------------|
| `starfish-reactor` | Async I/O reactor providing `UdpSocket` |
| `starfish-tls` | TLS/SSL support (shares `rustls` dependency) |
| `starfish-http` | HTTP/3 client and server built on this crate |
