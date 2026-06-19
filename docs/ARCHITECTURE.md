# Starfish Architecture

## Crate Dependency Graph

```
starfish-core          (foundation — no internal dependencies)
  │
  ├─→ starfish-reactor       (core)
  │     ├─→ starfish-tls     (core, reactor)
  │     └─→ starfish-epoch   (core, reactor)
  │           └─→ starfish-storage  (core, epoch)
  │
  ├─→ starfish-http          (reactor, quic, tls)
  │
  ├─→ starfish-quic          (reactor)
  │
  └─→ starfish-crossbeam     (core)

starfish-db            (future: storage, stream, wal-server, query)
```

## Crate Purposes

### starfish-core
Lock-free concurrent data structures and the trait abstractions that unify them. All collections are parameterized by a `Guard` type that determines the memory reclamation strategy, allowing the same data structure to run with epoch-based GC, deferred cleanup, or hazard pointers.

**Key types**: `SortedCollection<T, G>`, `MapCollection<K, V>`, `Guard`, `SkipList`, `SortedList`, `SplitOrderedHashMap`, `SkipTrie`, `Treap`, `YFastTrie`

### starfish-reactor
Cross-platform async I/O reactor with cooperative task scheduling. Each OS thread runs one `Reactor` that polls futures in a single-threaded loop. The `Coordinator` orchestrates multiple reactors across threads. Platform-specific I/O backends (io_uring, kqueue, IOCP) hide behind the `IOManager` trait.

**Key types**: `Reactor`, `Coordinator`, `IOManager`, `ExternalReactor`, `FutureRuntime`

### starfish-epoch
Custom epoch-based memory reclamation integrated with the Coordinator/Reactor runtime. Each reactor thread maintains an `EpochCounter` tracking its current epoch. Objects are deferred for deletion and reclaimed once all reactors have advanced past the deletion epoch.

**Key types**: `Epoch`, `EpochCounter`, `EpochAlloc`

### starfish-crossbeam
Memory-safe wrappers around starfish-core collections using crossbeam-epoch. Provides `EpochGuard` (implementing the `Guard` trait) and guarded collection types that handle pinning automatically.

**Key types**: `EpochGuard`, `EpochGuardedCollection`, `EpochGuardedHashMap`

### starfish-tls
Async TLS/SSL using rustls, integrated with the reactor's `AsyncRead`/`AsyncWrite` traits. Wraps TCP streams for transparent encryption.

**Key types**: `TlsClient`, `TlsServer`, `AcceptAnyServerCertVerifier`

### starfish-quic
QUIC transport protocol implementation (RFC 9000) built on `starfish-reactor`'s `UdpSocket` and `rustls` for TLS 1.3 (RFC 9001). Full packet I/O pipeline: AEAD encryption, header protection, loss detection, congestion control (NewReno + Cubic), stream multiplexing, and flow control.

**Key types**: `QuicConnection`, `QuicStream`, `QuicClient`, `QuicListener`, `QuicEndpoint`, `CongestionController`

### starfish-http
HTTP/1.1 and HTTP/3 client and server support. HTTP/1.1 includes a connection-pooling client, incremental request/response parsers, and chunked transfer encoding. HTTP/3 provides a QUIC-based client and server using QPACK header compression.

**Key types**: `H1Client`, `H1Server`, `H3Client`, `H3Server`, `HttpClient`, `HttpServer`, `HttpError`

### starfish-storage
Transactional storage engine for starfish-db. Designed to provide WAL, MVCC transactions, buffer pool, and indexes. Currently aspirational — see `starfish-storage/README.md` for the design.

### starfish-db
Transactional database with stream processing. Composes storage, reactor, and future crates (WAL server, query engine). Currently aspirational — see `starfish-db/README.md` for the design.

## Key Design Decisions

### 1. Guard Trait Abstraction

Collections don't own their memory reclamation strategy. Instead, they're generic over `G: Guard`, which provides:
- `pin()` → active read guard protecting node traversal
- `defer_destroy()` → schedule node for deferred deallocation
- `make_ref()` → create a guarded reference (zero-copy access)

This decouples data structure logic from GC strategy. The same `SkipList` works with `EpochGuard` (production), `DeferredGuard` (testing), or a future `HazardGuard`.

### 2. Lock-Free Collections via CAS

All sorted collections use Harris-style lock-free linked lists with CAS (compare-and-swap) for thread-safe modifications. Deleted nodes are logically marked (pointer bit-stealing) before physical removal, ensuring concurrent readers see consistent state.

Key optimization: **position-based batch inserts**. `NodePosition` captures predecessor pointers at all skip list levels, enabling O(1) amortized inserts for pre-sorted input.

### 3. Cooperative Scheduling (No Preemption)

Each reactor runs a single-threaded event loop. Futures yield cooperatively via `await`. There is no preemption, no work-stealing, no shared run queues. This eliminates synchronization overhead within a reactor thread.

The scheduling loop priority: IO completions > expired timers > active futures > external spawns.

### 4. Platform-Specific IO Behind IOManager Trait

```
IOManager (trait)
├── register_io_wait()    → submit async IO operation
├── completed_io()        → harvest completion
├── cancel_io_wait()      → cancel pending IO
└── has_active_io()       → check for pending work

Implementations:
├── UringIOManager     (Linux — io_uring SQE/CQE)
├── KqueueIOManager    (macOS — kqueue/kevent)
└── CompletionPortIOManager  (Windows — IOCP/OVERLAPPED)
```

### 5. Thread-Per-Core Model

One reactor per OS thread. No shared state between reactors except:
- `ExternalReactor::spawn_external()` — lock-free cross-reactor spawn via `SegQueue`
- `Epoch` global state — atomic epoch index with per-reactor counters

This is the Seastar/Glommio model: shared-nothing within a core, message-passing between cores.

### 6. Epoch-Based Memory Reclamation

Two implementations coexist:
- **starfish-crossbeam**: Uses crossbeam-epoch (production-ready, well-tested)
- **starfish-epoch**: Custom implementation integrated with the Coordinator lifecycle (has known bugs — see `starfish-epoch/docs/bugs.md`)

## Module Conventions

### starfish-core Layout
```
src/
├── data_structures/
│   ├── sorted/          # SkipList, SortedList, Treap
│   ├── hash/            # SplitOrderedHashMap
│   ├── trie/            # SkipTrie, YFastTrie
│   └── internal/        # Shared internals (marked_ptr, traits)
├── guard/               # Guard trait + implementations
├── preemptive_synchronization/  # CountdownEvent, FutureExtension
└── common_tests/        # Reusable test harness for SortedCollection impls
```

### starfish-reactor Layout
```
src/
├── cooperative_io/
│   ├── io_linux/        # io_uring backend
│   ├── io_macos/        # kqueue backend
│   ├── io_windows/      # IOCP backend
│   ├── async_read.rs    # AsyncRead + AsyncReadExtension traits
│   ├── async_write.rs   # AsyncWrite + AsyncWriteExtension traits
│   └── tcp_stream.rs    # TcpStream, TcpListener, UdpSocket
├── cooperative_synchronization/
│   ├── event_future.rs  # CooperativeEventFuture
│   ├── wait_one_future.rs  # CooperativeWaitOneFuture
│   ├── fair_lock.rs     # CooperativeFairLock
│   └── sleep.rs         # Delay, Sleep
├── reactor.rs           # Reactor (single-threaded event loop)
└── coordinator.rs       # Coordinator (multi-reactor orchestration)
```
