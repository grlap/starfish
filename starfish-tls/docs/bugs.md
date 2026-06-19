# starfish-tls Bug Tracker

Last updated: 2026-06-05

## High (from multi-lens review 2026-06-05)

### TLS-6: `AcceptAnyServerCertVerifier` is exported in the public API with no feature gate

**Files:** `src/lib.rs:7`, `src/accept_any_server_cert_verifier.rs`

`pub mod accept_any_server_cert_verifier` ships unconditionally in normal builds. The verifier returns `Ok` from `verify_server_cert`, `verify_tls12_signature`, and `verify_tls13_signature`, disabling **all** certificate and signature validation (trivial MITM). The only barrier against production misuse is the doc comment. rustls's own `.dangerous()` opt-in is a partial mitigation, but a TLS crate should not expose a MITM switch in its default public surface.

**Fix:** Gate the module behind a `dangerous-insecure-testing` Cargo feature (`#[cfg(feature = "dangerous-insecure-testing")]`), or move the verifier into a `dev-dependencies`-only test-utils crate. Add `#[deprecated(note = "disables ALL certificate verification; never use in production")]` for extra friction.

---

## Medium (from multi-lens review 2026-06-05)

### TLS-7: No caller-settable handshake timeout — a stalled peer hangs the connection indefinitely

**Files:** `src/tls_client.rs:57`, `src/tls_server.rs:45`

Both constructors hardcode `handshake_with_timeout(&None)`. With `None` the reactor registers no I/O deadline, so a peer that stalls mid-handshake blocks that task forever. The README advertises *"Timeout support — All operations support optional timeouts,"* which `new()` does not honor. (Cooperative scheduling makes this a per-connection hang, not a whole-reactor DoS.)

**Fix:** Add `handshake_timeout: Option<IOTimeout>` to `TlsClient::new` and `TlsServer::new`, thread it through to `handshake_with_timeout`, and add a test that connects to an unresponsive peer and asserts a timeout.

### TLS-8: README Quick Start example panics on `"host:port".parse()`

**File:** `README.md:38`

`TcpStream::connect("example.com:443".parse().unwrap())` panics at runtime — `SocketAddr::from_str` does not resolve DNS, so a hostname:port string yields "invalid socket address syntax". This is the first line a user copies. The tests correctly use `to_socket_addrs()`.

**Fix:** Use an IP literal (`"127.0.0.1:443".parse().unwrap()`) or demonstrate DNS resolution via `"example.com:443".to_socket_addrs()?.next()` with `use std::net::ToSocketAddrs;`.

### TLS-9: Certificate-rejection path has no test coverage

**File:** `tests/tls_client_test.rs:37-55`

Every integration test installs `AcceptAnyServerCertVerifier` via `create_client_config()`, so a bad/self-signed cert *failing* the handshake into `TlsError::Tls` is never exercised — the security-critical validation path is unproven.

**Fix:** Add `test_certificate_rejection`: build a `ClientConfig` with a real `RootCertStore` and **without** `.dangerous().set_certificate_verifier(...)`, connect to a server using a self-signed cert not in the store, and assert `TlsClient::new` returns `TlsError::Tls`.

### TLS-10: `TlsClient` and `TlsServer` duplicate ~150 lines of handshake/read/write logic

**Files:** `src/tls_client.rs:64-306`, `src/tls_server.rs:51-289`

The handshake, read, and write implementations are near byte-for-byte identical, differing only in `ClientConnection` vs `ServerConnection`. This is the structural reason several nits below must be fixed twice.

**Fix:** Extract a generic `TlsConnection<C>` over the shared rustls connection surface (`read_tls`/`write_tls`/`process_new_packets`/`reader`/`writer`/`is_handshaking`), implement `AsyncRead`/`AsyncWrite` once, and alias `TlsClient = TlsConnection<ClientConnection>` / `TlsServer = TlsConnection<ServerConnection>`.

### TLS-11: Zero-length read buffer hangs instead of returning `Ok(0)`

**Files:** `src/tls_client.rs:148-176`, `src/tls_server.rs:131-159`

With `buf.len() == 0`, the `buf_len > 0` guard is never satisfied, so `read_with_timeout` falls through to a blocking socket read and spins/blocks forever instead of returning immediately — a violation of the `Read` convention (empty buffer ⇒ `Ok(0)`).

**Fix:** Add `if buf.is_empty() { return Ok(0); }` at the top of both read paths, plus a regression test calling `read(&mut [])` that asserts `Ok(0)` without blocking.

---

## Low (from multi-lens review 2026-06-05)

### TLS-12: Unclean close (truncation) is masked as a clean EOF

**Files:** `src/tls_client.rs:185-199`, `src/tls_server.rs:168-182`

On stream EOF the `_ => return Ok(buf_len)` arm swallows the `Err(UnexpectedEof)` that rustls's `Reader::read` returns when the peer sends TCP FIN without a `close_notify` alert (see `rustls-0.23/src/conn.rs:219-221`). The actual masking is this catch-all arm, not the adjacent `let _ = process_new_packets()`. Impact is low — treating unclean EOF as end-of-stream is common, and only matters for protocols that rely on TLS close for framing (HTTP/1.1 with Content-Length/chunked is self-delimiting) — but a general-purpose TLS wrapper should let the caller decide.

**Fix:** In the EOF arm, distinguish `Err(e) if e.kind() == ErrorKind::UnexpectedEof` (surface it) from `WouldBlock`/`Ok(0)` (clean close), or document the lenient behavior explicitly. Add a test that closes a peer with TCP FIN but no `close_notify`.

### TLS-13: Inconsistent error suppression on the pre-socket plaintext-drain arm

**Files:** `src/tls_client.rs:166-171`, `src/tls_server.rs:149-154`

The pre-socket plaintext drain uses `match reader().read(...) { Ok(c) if c > 0 => ..., _ => {} }`, swallowing `Err` along with `Ok(0)`. This is inconsistent with the in-loop drain (`tls_*.rs` later in the same method) which propagates errors. Low impact — `reader()` reads already-decrypted in-memory data — but a TLS library should not blanket-discard errors.

**Fix:** Split the catch-all into explicit arms: keep `Ok(0)` a no-op, and either propagate `Err` or add an `Err(_) => {}` with a comment documenting the intentional best-effort semantics. Apply to both files.

### DOC-1: Documentation and convention gaps

Bundled small documentation fixes:

- **`src/tls_client.rs:19`** — the struct doc uses `//` not `///`, so public `TlsClient` has no rustdoc (`TlsServer` at `src/tls_server.rs:19` is correct). Convert to `///`.
- **`README.md`** — the Core Types section omits `TlsError` and its variants (`InvalidServerName`, `Tls`, `Io`) plus the `From` impls that make `?` work; add a subsection.
- **`src/lib.rs:12-14`** — the `MIN_READ_BUF_SIZE` comment says "16 KB plus overhead," but the value is exactly 16384 and the buffer holds *ciphertext*; reword (note: 16384 deliberately matches rustls's `received_plaintext` limit — see NB-1).
- **`docs/DESIGN.md`** — the crate has no design doc and no `docs/DESIGN-INDEX.md` entry beyond the bug tracker; add one covering the connection lifecycle, reactor/timeout integration, and known constraints (no built-in client-cert auth, no session resumption).

### TST-1: Test coverage gaps

Bundled missing tests (each a small add):

- **Timeout path** — no test exercises a non-`None` `IOTimeout` on read/write/handshake (entirely untested; pairs with TLS-7).
- **ALPN** — `alpn_protocol()` is only asserted `is_none()`; add a test that configures matching ALPN on both sides and asserts successful negotiation.
- **Mutual TLS** — `peer_certificates()` is only asserted `None`; add a `with_client_auth_required()` test that verifies both sides extract the chain.
- **EOF during handshake** — the `count == 0` handshake path (`UnexpectedEof "EOF during handshake"`) is never triggered.
- **Truncation** — only `test_clean_shutdown` (close_notify present) exists; add an abrupt-TCP-close test (pairs with TLS-12).

---

## Info (from multi-lens review 2026-06-05)

### TLS-14: `Cargo.toml` lacks `description` / `license` / `repository`

**File:** `Cargo.toml`

The `[package]` section omits standard metadata. This is workspace-wide (most crates share the gap), so it is best addressed via workspace inheritance in the root `Cargo.toml` rather than per-crate.

---

## Investigated — not bugs (multi-lens review 2026-06-05)

Two findings raised during review were checked against the rustls 0.23.32 source and are **not** bugs. Recorded here so they are not re-flagged.

### NB-1: "`read_tls` partial-consumption drops bytes" in the read loop — NOT a bug

**Files:** `src/tls_client.rs:208-238`, `src/tls_server.rs:191-221`

The concern was that `if n == 0 { break; }` after `read_tls` could abandon unconsumed ciphertext when `read_buf` is overwritten on the next iteration. rustls's `read_tls` is **byte-oriented**, not record-oriented: `prepare_read` always grows the deframer buffer to make room, and backpressure (`received_plaintext` full / message buffer full) is reported as an **`Err`** — which the code already propagates via `map_err(io::Error::other)?` — not as `Ok(0)`. `read_tls` returns `Ok(0)` with bytes still in the slice **only after `close_notify`**, where discarding the tail is correct. Verified against `rustls-0.23.32/src/conn.rs:760` (`read_tls`) and `src/msgs/deframer/buffers.rs:199` (`read`/`prepare_read`). `MIN_READ_BUF_SIZE` (16384) also matches rustls's 16 KB `received_plaintext` limit, keeping the small-caller-buffer case bounded.

### NB-2: Handshake "drops coalesced TLS records" by calling `read_tls` once — NOT a bug

**Files:** `src/tls_client.rs:87-102`, `src/tls_server.rs:72-85`

The concern was that the handshake calls `read_tls` once per network read (unlike the read path's offset loop) and would lose records when a single read delivers a coalesced TLS 1.3 flight. Because `read_tls` is byte-oriented, a single call consumes the entire 4096-byte buffer (the handshake deframer allows up to 64 KB), and `process_new_packets` then parses **all** complete records buffered so far; handshake messages split across multiple socket reads accumulate across loop iterations. The read path's offset loop is defensive, not a correctness requirement the handshake is missing.
