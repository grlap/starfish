# Starfish

**Starfish** is a high-performance, cross-platform asynchronous I/O and cooperative multitasking runtime for Rust. It is designed to provide efficient, non-blocking I/O operations and concurrency primitives using native OS facilities (io_uring on Linux, kqueue on macOS, IOCP on Windows).

## Project Structure

This repository is a workspace containing several crates that make up the Starfish ecosystem:

- **[starfish-core](./starfish-core)**: Core data structures and utilities used across the project.
- **[starfish-reactor](./starfish-reactor)**: The heart of the runtime, providing the event loop, task scheduling, and cross-platform I/O reactor.
- **[starfish-http](./starfish-http)**: HTTP protocol support.
- **[starfish-tls](./starfish-tls)**: TLS/SSL integration.
- **[starfish-epoch](./starfish-epoch)**: Epoch-based memory reclamation for lock-free data structures.
- **[starfish-storage](./starfish-storage)**: Storage abstractions.
- **[starfish-crossbeam](./starfish-crossbeam)**: Integration with Crossbeam tools.

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
