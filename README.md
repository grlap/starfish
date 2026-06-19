# Starfish

**Starfish** is a high-performance, cross-platform asynchronous I/O and cooperative multitasking runtime for Rust. It is designed to provide efficient, non-blocking I/O operations and concurrency primitives using native OS facilities (io_uring on Linux, kqueue on macOS, IOCP on Windows).

## Project Structure

This repository is a workspace containing several crates that make up the Starfish ecosystem:

- **[starfish-core](./starfish-core)**: Lock-free data structures (SkipList, SortedList, SplitOrderedHashMap, SkipTrie, YFastTrie, Treap) and synchronization primitives.
- **[starfish-crossbeam](./starfish-crossbeam)**: Epoch-based memory reclamation for starfish collections using crossbeam-epoch.
- **[starfish-epoch](./starfish-epoch)**: Custom epoch-based memory reclamation integrated with the Coordinator/Reactor runtime.
- **[starfish-reactor](./starfish-reactor)**: Cross-platform async I/O reactor (io_uring, kqueue, IOCP) and cooperative task scheduler.
- **[starfish-http](./starfish-http)**: HTTP/1.1 and HTTP/3 client and server (RFC 9112, RFC 9114).
- **[starfish-quic](./starfish-quic)**: QUIC transport protocol (RFC 9000, RFC 9001, RFC 9002).
- **[starfish-tls](./starfish-tls)**: Async TLS/SSL using rustls.
- **[starfish-storage](./starfish-storage)**: Transactional storage engine (WAL, MVCC, buffer pool, indexes).
- **[starfish-db](./starfish-db)**: Transactional database with stream processing, composing the storage and reactor crates.

## Key Features

- **True Cross-Platform Async I/O**: Leverages `io_uring` (Linux), `kqueue` (macOS), and `IOCP` (Windows) for optimal performance.
- **Cooperative Multitasking**: Efficient userspace task scheduling.
- **Networking**: Async TCP and UDP support.
- **File I/O**: Asynchronous file operations.
- **Synchronization**: Async mutexes, events, and other primitives.

## Getting Started

To use the Starfish reactor in your project, add `starfish-reactor` to your dependencies.

### Running Tests

You can run tests for the entire workspace:

```bash
cargo test
```

### Benchmarks

Run benchmarks using:

```bash
cargo bench
```

## Documentation

For detailed examples and API usage, please refer to the **[starfish-reactor README](./starfish-reactor/README.md)**.
