# Starfish-QUIC Bug Report

## HTTP/3 Bring-up Status

Local regression coverage is green (`cargo test -p starfish-quic --tests`, `cargo test -p starfish-http --tests h3`). All transport bugs found in the 2026-03 interop and crate reviews are fixed locally; those resolved entries have been trimmed from this tracker and remain in git history. (For the record: unknown QUIC transport frame types are correctly treated as frame-encoding errors at the transport layer, while unknown HTTP/3 frames are ignored in `starfish-http` per RFC 9114.)

All previously tracked QUIC transport blockers for a strong "full working HTTP/3" claim are fixed locally. The remaining step is independent-stack validation: running the default HTTP/3 suite on a machine with external `aioquic` and `quiche` tooling available so the self-skipping interop lanes actually execute, or a Rust-native harness embedding an independent stack such as `quiche`. Separate-process CLI coverage is additional assurance, not a separate tracked blocker.

## Crate review 2026-06-05 (open)

A multi-lens correctness/security/RFC-compliance review of `starfish-quic` opened `QUIC-147` through `QUIC-159` (3 high, 5 medium, 3 low, 2 informational). None are memory-safety/RCE; they are RFC-compliance gaps and resource-exhaustion (DoS) vectors. The High items — missing per-space frame validation (`QUIC-147`), and two unbounded-table DoS vectors (`QUIC-148`, `QUIC-149`) — are the priorities. A recurring theme: the correct error codes already exist but are never raised (`CONNECTION_ID_LIMIT_ERROR` for `QUIC-150`, `CRYPTO_BUFFER_EXCEEDED` for `QUIC-151`). Full entries are at the end of this file.

## Production Readiness Roadmap (assessed 2026-06-06)

**Verdict: maturity ~2/5 — feature-broad, `unsafe`-free, correctness-plausible in controlled environments, but not production-ready for untrusted/adversarial networks.** The gap is validation maturity (adversarial hardening, fuzzing, proven interop), not feature breadth. A multi-dimension assessment (protocol conformance, robustness, interop, security, performance/ops) independently converged on 2/5.

**Estimated remaining effort: ≈6–12 weeks of focused AI-assisted development (one engineer driving Claude Code), recalibrated from a 24–38 person-week human baseline.** AI collapses the *authoring* cost — bug fixes, adversarial test modules, fuzz/bench harness scaffolding, instrumentation, docs — by roughly an order of magnitude. What it does *not* collapse are the validation floors that set the production bar: fuzz soak time (runtime-bound, though parallelizable across the high-core dev machines), cross-implementation interop debugging against real external stacks, congestion/perf measurement loops on real hardware, and human review of security- and concurrency-critical diffs (exactly the class of plausible-but-subtly-wrong change the adversarial reviews here exist to catch). The bottleneck shifts from *writing* QUIC code to *producing external evidence that it is correct* — so interop (Phase 2) dominates the residual cost and carries the widest variance. Maturity stays 2/5: that is a current-state rating, independent of who does the work. Excludes an external security audit (add separately if required before internet exposure).

### Already strong
- Broad RFC 9000/9001/9002 coverage: 0-RTT, key update, migration + path validation, preferred address, ECN, path-MTU discovery, stateless reset, Retry + NEW_TOKEN, version negotiation, spin bit, datagrams. (No QUIC v2 / RFC 9369.)
- Zero `unsafe`; 325 internal tests; crypto delegated to `rustls`/`ring`; anti-amplification (3×) implemented.

### Biggest gates to a production claim
1. **The 3 HIGH bugs** (`QUIC-147`, `QUIC-148`, `QUIC-149`) — per-space frame validation and two pre-auth memory-DoS vectors. Block any honest production or interop claim.
2. **Cross-implementation interop at the transport layer** — currently zero external evidence; the only interop is HTTP/3-layer, self-skipping, and never runs in CI.
3. **A QUIC fuzzing harness** — `starfish-http` has one; quic has none. Untrusted-input safety is unproven.
4. **The cheap-but-required robustness bugs** where the right error code exists but is never raised (`QUIC-150`, `QUIC-151`, plus `QUIC-152`/`QUIC-153`/`QUIC-154`).
5. **Perf/ops validation** — no benchmarks, congestion control (NewReno/Cubic) never tested under loss/ECN, observability is `eprintln` behind an env var.

### Phase 1 — Correctness & DoS hardening (human ~7–10 wk → AI-assisted ~1–2 wk)
*Prerequisite for everything else: fuzzing and interop will trip over these immediately. Highly compressible — most of QUIC-147…159 have known fixes; the residual cost is design judgment (connection-cap policy, `peer_paths` eviction) and careful review of the security/concurrency-critical diffs.*
- Fix the 3 HIGH bugs first: `QUIC-147` (`frame_allowed_in_space()` gate → `PROTOCOL_VIOLATION`), `QUIC-148` (bound/defer `peer_paths`), `QUIC-149` (per-endpoint connection cap and/or default `require_retry=true` / stateless Initial pre-validation).
- Fix the medium DoS/conformance bugs: `QUIC-150` (enforce `active_connection_id_limit` → `CONNECTION_ID_LIMIT_ERROR`), `QUIC-151` (cap CRYPTO reassembly → `CRYPTO_BUFFER_EXCEEDED`), `QUIC-152` (ACK-of-unsent → `PROTOCOL_VIOLATION`), `QUIC-153` (stream-count TP ≤ 2^60 → `TRANSPORT_PARAMETER_ERROR`), `QUIC-154` (clamp ACK delay to peer `max_ack_delay`).
- Fix the low/info items: `QUIC-155`–`QUIC-159`.
- Add a dedicated malformed/adversarial test module (per-space frame coverage, CRYPTO offset off-by-ones, ACK range ordering/dupes, stream-ID parity, MAX_STREAMS at 2^60, TP edge cases, PN wraparound).
- Add a key-update / AEAD confidentiality-limit end-to-end test; document the CSPRNG guarantee.

### Phase 2 — Fuzzing & cross-implementation interop (human ~12–18 wk → AI-assisted ~4–7 wk)
*The dominant, partly-overlapping cost centers, and the LEAST compressible by AI. Harness scaffolding is AI-fast (days), but fuzz soak is runtime-bound and interop is an iterative debug loop against real external stacks — AI speeds triage/wire-trace analysis but cannot remove the wall-clock. This is where the residual estimate concentrates.*
- Stand up `cargo-fuzz` for `starfish-quic` mirroring `starfish-http/fuzz`: targets for packet-header parse, frame decode (all ~20 types), transport-parameter parse, and Initial→Handshake→1-RTT state transitions; run with sanitizers + crash minimization; wire into nightly CI and triage findings.
- Integrate `quic-interop-runner` (or a Rust-native differential harness embedding `quiche`) at the **transport** layer (not just HTTP/3), against ≥2 independent stacks (quiche, aioquic, quinn, ngtcp2).
- Provide CI tooling so the existing self-skipping `h3_interop_*` lanes actually execute, and add a transport-interop CI lane that fails on regressions.
- Cover the interop matrix: handshake, retry, resumption, 0-RTT, multiplexing, loss recovery, migration, key update, ECN, amplification — fix all surfaced failures.

### Phase 3 — Performance & operational readiness (human ~6–11 wk → AI-assisted ~2–3 wk)
*Lower priority than 1–2, but required before any predictable-performance or operability claim. Benches, `tracing`, config surface, and docs are AI-fast; the residual is measurement loops on real hardware (CC validation under loss/ECN, perf tuning) that AI accelerates analysis on but cannot fully collapse.*
- Criterion benchmarks: packet encode/decode, frame processing, congestion-control steps, stream I/O, handshake time, key-update overhead; establish baselines vs a reference stack.
- Validate congestion control (NewReno + Cubic) under injected loss/ECN/reordering; verify window growth/collapse, multi-RTT recovery, and that pacing reduces burstiness.
- Add structured observability: optional `tracing` feature (spans/events for handshake-complete/loss/key-update) and a `Stats` snapshot exposing RTT, cwnd, bytes-in-flight, ECN counts (replace `eprintln` env-var tracing).
- Expand the operational config surface (idle timeout, initial cwnd, `max_ack_delay`, ECN toggle, preferred MTU, burst size) with documented LAN/WAN/mobile profiles; expose graceful-shutdown/drain semantics; audit hot-path allocations; consider GSO/batched I/O once throughput is the bottleneck.
- Write `OPERATIONS.md` with latency/throughput expectations and tuning guidance; bump the crate version past 0.1.0 once the gates clear.

## High (from crate review 2026-06-05)

### QUIC-147. Received frames are not validated against the packet-number space - connection.rs:2875-3305

`process_frame_from()` restricts frame types only for 0-RTT packets (the `frame_allowed_in_zero_rtt` check at the top of the dispatch). Frames decoded from Initial and Handshake packets are then dispatched unconditionally, so a malicious peer — or, for the Initial space, an off-path attacker who derives Initial keys from the cleartext DCID per RFC 9001 §5.2 — can inject application-space frames (`RESET_STREAM`, `STOP_SENDING`, `STREAM`, `MAX_DATA`, `NEW_CONNECTION_ID`, `RETIRE_CONNECTION_ID`, `PATH_CHALLENGE`, `PATH_RESPONSE`, `HANDSHAKE_DONE`, `NEW_TOKEN`, …) inside Initial/Handshake packets. RFC 9000 §12.4 requires receipt of a frame in a packet type that does not permit it to be a connection error of type `PROTOCOL_VIOLATION`. Worst case: the `HANDSHAKE_DONE` arm guards only on `side == Side::Client`, not on the packet-number space, so a client that receives `HANDSHAKE_DONE` inside an **Initial** packet sets `state = Connected` and discards Handshake keys mid-handshake, corrupting connection state.

**RFC:** RFC 9000 §12.4 (frame-type vs. packet-type permissions; Table 3)

**Key files:** `src/connection.rs`

**Suggested fix:** add a `frame_allowed_in_space(frame, space)` helper and call it at the top of `process_frame_from()`, rejecting frames not permitted in the Initial/Handshake spaces with `PROTOCOL_VIOLATION` (only `PADDING`, `PING`, `ACK`, `CRYPTO`, `CONNECTION_CLOSE` of type 0x1c are allowed there).

### QUIC-148. Unbounded `peer_paths` HashMap allows pre-authentication memory DoS - connection.rs:282, 459-495, 2419

`record_unvalidated_bytes_received()` is the first statement of `process_datagram_from_with_ecn()` — executed before header parsing and before packet decryption/authentication — and creates a `PeerPathState` entry for every previously unseen source address via `peer_paths.entry(peer_addr).or_default()`. There is no cap, pruning, LRU eviction, or cleanup on the `peer_paths` map anywhere (verified across all of its use sites). In standalone `QuicListener` mode there is no DCID gate, so an attacker spoofing UDP source addresses can grow the map without bound; in `QuicEndpoint` mode the cleartext DCID is observable on-path, after which a spoofed source still creates a pre-authentication entry. Per-entry cost is small, so this is a high-volume memory-exhaustion DoS rather than an instant crash.

**RFC:** RFC 9000 §8.2 (path validation); §9 (connection migration). Per-path state limiting is a defensive measure, not a strict MUST.

**Key files:** `src/connection.rs`

**Suggested fix:** bound `peer_paths` with LRU eviction, or only create path state after a successful `PATH_RESPONSE` validation; at minimum defer entry creation until after packet authentication succeeds.

### QUIC-149. Unbounded endpoint connection table with `require_retry` defaulting to false allows memory DoS - server.rs:734-852

`QuicEndpoint::drive_once()` allocates a full `rustls::quic::ServerConnection` plus `QuicConnection` for every Initial packet carrying a fresh DCID and inserts it into the `connections` map with no size check. `QuicServerConfig` defaults `require_retry = false`, so an empty-token Initial yields `Proceed { peer_address_validated: false }` and a new connection is created without address validation. The only reclamation is the ~30s idle timeout; the 3× anti-amplification cap limits bytes sent per path, not the number of connection objects. A spoofing attacker can therefore accumulate large numbers of TLS-bearing connections (memory-exhaustion DoS) before idle cleanup balances inflow.

**RFC:** RFC 9000 §8.1.2 (address validation via Retry); §21 (resource-exhaustion / amplification considerations).

**Key files:** `src/server.rs`

**Suggested fix:** enforce a per-endpoint connection cap before creating new connections, and/or default `require_retry` to true (or require an explicit opt-in to creating unvalidated connections). Consider stateless validation of the Initial before allocating a full connection.

---

## Medium (from crate review 2026-06-05)

### QUIC-150. `active_connection_id_limit` is never enforced on peer CIDs - connection.rs:3166-3195, transport/connection_id.rs:136-142

The `NEW_CONNECTION_ID` handler validates `retire_prior_to <= sequence_number` and CID length (1..=20) but never checks the number of active (non-retired) peer CIDs against the `active_connection_id_limit` we advertised. `ConnectionIdManager::add_peer_cid()` pushes each distinct-sequence CID into the unbounded `peer` Vec and always returns `Ok`. A peer sending `NEW_CONNECTION_ID` for sequence numbers 0,1,2,…,N with `retire_prior_to = 0` retires nothing, so all CIDs accumulate. RFC 9000 §5.1.1 requires closing with `CONNECTION_ID_LIMIT_ERROR` when the active count exceeds the advertised limit; the `ConnectionIdLimitError` (0x09) variant is defined in `error.rs` but never raised anywhere in the crate. This is both an RFC-compliance gap and a slow memory-growth DoS.

**RFC:** RFC 9000 §5.1.1 (`CONNECTION_ID_LIMIT_ERROR` MUST-close requirement).

**Key files:** `src/connection.rs`, `src/transport/connection_id.rs`

**Suggested fix:** after `add_peer_cid()`, count non-retired peer CIDs and close with `CONNECTION_ID_LIMIT_ERROR` if it exceeds our advertised limit (default 2). Note the connection currently stores only `peer_active_connection_id_limit`; our own advertised value (built into the local transport parameters) must be retained for the comparison.

### QUIC-151. Unbounded CRYPTO frame reassembly buffer during handshake - connection.rs:3324-3336

`process_crypto_data()` buffers out-of-order CRYPTO frames into a per-space `BTreeMap<u64, Vec<u8>>` keyed by offset with no total-size accounting and no rejection path. `decode_crypto_frame` validates only against truncation, not the offset magnitude (a full u64). A peer sending CRYPTO chunks at ever-increasing offsets (never delivering offset 0) leaves the in-order drain loop stuck while the buffer grows without bound — a memory-exhaustion DoS during the handshake, most acute server-side across many concurrent handshakes. RFC 9000 §7.5 specifies `CRYPTO_BUFFER_EXCEEDED` for exactly this; the 0x0d variant is defined in `error.rs` but never raised.

**RFC:** RFC 9000 §7.5 (cryptographic message buffering; `CRYPTO_BUFFER_EXCEEDED` = 0x0d).

**Key files:** `src/connection.rs`

**Suggested fix:** track buffered bytes per space and reject with `CRYPTO_BUFFER_EXCEEDED` once a reasonable cap (e.g. 64 KiB) is exceeded.

### QUIC-152. No `PROTOCOL_VIOLATION` on acknowledgement of an unsent packet - recovery/loss_detection.rs:206-237, connection.rs:2894-2973

`on_ack_received()` resolves ACK ranges via `sent_packets.range(...)`, so acknowledgements for packet numbers that were never sent are silently ignored, and the RTT update for an unsent `largest_acknowledged` is skipped. The `Frame::Ack` handler never compares `largest_acknowledged` against `next_pn[space]` (the highest packet number ever sent). RFC 9000 §13.1 says an endpoint SHOULD treat receipt of an acknowledgement for a packet it did not send as a connection error of type `PROTOCOL_VIOLATION`.

**RFC:** RFC 9000 §13.1 (packet processing; ACK of unsent packet — SHOULD).

**Key files:** `src/recovery/loss_detection.rs`, `src/connection.rs`

**Suggested fix:** in the ACK handler, reject `largest_acknowledged >= next_pn[space]` with `PROTOCOL_VIOLATION`.

### QUIC-153. Stream-count transport parameters not bounded to 2^60 - connection.rs:535-542, 805-898

`validate_peer_transport_parameters_for_state()` does not bound `initial_max_streams_bidi` / `initial_max_streams_uni`; the parser (`packet/mod.rs`) accepts them up to the varint maximum (2^62-1) and `apply_peer_initial_limits` applies them unclamped. RFC 9000 §4.6 requires closing with `TRANSPORT_PARAMETER_ERROR` when a max_streams transport parameter exceeds 2^60. The `MAX_STREAMS` *frame* path already enforces this `2^60` limit (rejecting with `FRAME_ENCODING_ERROR`); the transport-parameter path was missed. Current exploitability is low (the value is an advisory ceiling on locally opened streams), but it is a clear MUST-level conformance gap.

**RFC:** RFC 9000 §4.6 (`TRANSPORT_PARAMETER_ERROR` for max_streams > 2^60); §18.2.

**Key files:** `src/connection.rs`

**Suggested fix:** reject `initial_max_streams_bidi`/`initial_max_streams_uni > MAX_STREAMS_LIMIT` (1 << 60) with `TRANSPORT_PARAMETER_ERROR`, mirroring the frame-path check.

### QUIC-154. Incoming ACK delay not clamped to peer's `max_ack_delay` - connection.rs:2905-2920

The decoded ACK delay is scaled by 2^ack_delay_exponent and passed to the RTT estimator without clamping to the peer's `max_ack_delay`. RFC 9002 §5.3 requires using the lesser of the reported Ack Delay and the peer's `max_ack_delay` after the handshake is confirmed. Practical impact is limited because the companion §5.3 MUST (don't subtract the delay if the result would fall below `min_rtt`) *is* implemented at `loss_detection.rs:94`, so an inflated delay can only skip the subtraction, not drive RTT below `min_rtt` — but the clamp itself is missing. `peer_max_ack_delay` is already stored and currently consumed only for PTO.

**RFC:** RFC 9002 §5.3 (use lesser of Ack Delay and peer's `max_ack_delay`).

**Key files:** `src/connection.rs`

**Suggested fix:** clamp the decoded delay with `.min(peer_max_ack_delay)` before `on_ack_received()` for ApplicationData; Initial/Handshake spaces should ignore Ack Delay entirely.

---

## Low (from crate review 2026-06-05)

### QUIC-155. Initial-packet congestion-window estimate excludes padding - connection.rs:3838-3870

In `build_packet_to_addr()`, the congestion-window admission check (`can_send`) uses the unpadded payload size, but Initial packets are padded to `INITIAL_MIN_DATAGRAM_SIZE` (1200) afterward and `on_packet_sent()` charges the padded size. The check therefore under-counts an in-progress Initial packet by up to ~1080 bytes. The discrepancy is self-correcting (bytes-in-flight is reconciled per datagram, and prior coalesced padded packets are accounted via `pending_datagram_bytes`) and bounded relative to the 12000-byte initial window, so the worst case is roughly a one-packet overshoot.

**RFC:** RFC 9002 §2 ("in flight" includes padding); §7 / Appendix B.2 (`OnPacketSent`).

**Key files:** `src/connection.rs`

**Suggested fix:** include the Initial padding delta in `estimated_size` before the congestion check.

### QUIC-156. Unchecked multiplication in Cubic window increment - recovery/cubic.rs:135

`cubic.rs` computes `((w_cubic_bytes - cwnd) * bytes) / cwnd` with an unchecked `usize` multiply and no upper clamp on `w_cubic_bytes` (the f64→usize cast only lower-bounds at 0.0). On 64-bit targets overflow requires an unrealistic ~8h single uninterrupted congestion-avoidance epoch (any loss/ECN/persistent-congestion resets the epoch); it is only practically reachable on 32-bit targets, which this project does not support. Latent correctness hardening — release builds would wrap silently (no `overflow-checks` in the release profile).

**RFC:** RFC 8312 §4.2 / RFC 9438 (CUBIC window increment) — formula is correct; this is purely an integer-overflow concern.

**Key files:** `src/recovery/cubic.rs`

**Suggested fix:** use `saturating_mul` (or compute the product in u128) for the increment.

### QUIC-157. Non-constant-time stateless-reset-token comparison - transport/connection_id.rs:225-229

`ConnectionIdManager::is_peer_stateless_reset_token()` compares the 16-byte token using the derived `==` on `Option<[u8; 16]>`, which short-circuits and is not constant-time. This is not an RFC requirement and is impractical to exploit (128-bit random tokens, and the boolean result gives no per-guess leading-match oracle), but the crate already uses `ring::constant_time::verify_slices_are_equal` for Retry-tag comparison (`packet/retry.rs:293`), so this is a cheap consistency fix.

**RFC:** RFC 9000 §10.3.1 (detecting a stateless reset) — imposes no constant-time requirement; defense-in-depth only.

**Key files:** `src/transport/connection_id.rs`

**Suggested fix:** compare tokens with `ring::constant_time::verify_slices_are_equal`.

---

## Informational (from crate review 2026-06-05)

### QUIC-158. No packet-number exhaustion check on the send path - connection.rs:3974

`build_packet_to_addr()` increments `next_pn[space]` with `+= 1` and never checks the RFC 9000 §12.3 limit of 2^62-1, at which a sender MUST close the connection without sending a `CONNECTION_CLOSE` or any further packets. Unreachable in practice (would require sending 2^62 packets — millions of years at realistic rates) and well below `u64` overflow, so no panic occurs; pure conformance gap.

**RFC:** RFC 9000 §12.3 (packet-number exhaustion).

**Key files:** `src/connection.rs`

**Suggested fix:** close the connection (without sending further packets) when `next_pn[space]` reaches 2^62-1.

### QUIC-159. Unchecked sequence-number increment in `issue_new_cid` - transport/connection_id.rs:96

`ConnectionIdManager::issue_new_cid()` increments `next_sequence` with `+= 1` and never bounds it. Overflow requires ~2^64 CID issuances (infeasible — each attacker-driven `RETIRE_CONNECTION_ID` advances it by at most 1); it would panic in debug/test builds (`overflow-checks = true`) or wrap in release. Theoretical hardening only.

**RFC:** RFC 9000 §5.1.1 (sequence numbers MUST increase by 1; no maximum defined); §19.15 (encoded as a varint, max 2^62-1).

**Key files:** `src/transport/connection_id.rs`

**Suggested fix:** use `checked_add` and close with an internal error on overflow, or document the bound.
