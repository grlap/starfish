# starfish-reactor

Cross-platform async I/O reactor with cooperative multitasking for the Starfish ecosystem.

## Overview

`starfish-reactor` provides a high-performance, platform-independent async I/O reactor designed for cooperative multitasking. It leverages the best native I/O mechanisms on each platform (io_uring on Linux, kqueue on macOS, IOCP on Windows) while exposing a unified async API.

## Features

- **Cross-platform async I/O** - Unified API across Linux, macOS, and Windows
- **Platform-optimized backends** - io_uring (Linux), kqueue (macOS), IOCP (Windows)
- **Cooperative scheduling** - Efficient task scheduling with yield support
- **Multi-reactor coordination** - Orchestrate multiple reactors across threads
- **Cancelable I/O** - Timeout support for all I/O operations
- **TCP/UDP networking** - Full async networking support
- **File I/O** - Async file read/write operations
- **Synchronization primitives** - Events, locks, and delayed futures

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      Coordinator                             │
│   (Multi-reactor orchestration, thread management)           │
└─────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│   Reactor 0   │     │   Reactor 1   │     │   Reactor N   │
│  (Thread 0)   │     │  (Thread 1)   │     │  (Thread N)   │
└───────────────┘     └───────────────┘     └───────────────┘
        │                     │                     │
        ▼                     ▼                     ▼
┌───────────────────────────────────────────────────────────┐
│                      IOManager                             │
│  ┌──────────────┬──────────────┬──────────────┐           │
│  │   io_uring   │    kqueue    │     IOCP     │           │
│  │   (Linux)    │   (macOS)    │  (Windows)   │           │
│  └──────────────┴──────────────┴──────────────┘           │
└───────────────────────────────────────────────────────────┘
```

## Platform Backends

| Platform | Backend | Description |
|----------|---------|-------------|
| Linux | io_uring | High-performance kernel async I/O (requires kernel 5.1+) |
| macOS | kqueue | BSD event notification interface |
| Windows | IOCP | I/O Completion Ports |

## Quick Start

### Single Reactor

```rust
use starfish_reactor::reactor::Reactor;
use starfish_reactor::cooperative_io::file_open_options::FileOpenOptions;
use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;

fn main() {
    let mut reactor = Reactor::new();

    reactor.spawn(async {
        // Open and read a file asynchronously
        let mut file = FileOpenOptions::new()
            .read(true)
            .open("example.txt")
            .unwrap();

        let mut buffer = vec![0u8; 1024];
        let bytes_read = file.read(&mut buffer).await.unwrap();
        println!("Read {} bytes", bytes_read);
    });

    reactor.run();
}
```

### Multi-Reactor with Coordinator

```rust
use starfish_reactor::coordinator::Coordinator;

fn main() {
    let mut coordinator = Coordinator::new();

    // Initialize 4 reactor threads
    coordinator.initialize(4);

    // Spawn work on specific reactors
    let futures: Vec<_> = coordinator.for_each_reactor(|| async {
        println!("Running on reactor!");
    });

    // Wait for all futures
    for future in futures {
        // Handle completion
    }

    coordinator.join_all().unwrap();
}
```

## Core Components

### Reactor

The main event loop that executes async futures cooperatively:

```rust
use starfish_reactor::reactor::{Reactor, cooperative_yield};

let mut reactor = Reactor::new();

// Spawn a future
reactor.spawn(async {
    println!("Task 1 - part 1");
    cooperative_yield().await;  // Yield to other tasks
    println!("Task 1 - part 2");
});

// Spawn another future
reactor.spawn(async {
    println!("Task 2");
});

// Run the reactor
reactor.run();
```

### Coordinator

Manages multiple reactors across threads:

```rust
use starfish_reactor::coordinator::Coordinator;

let mut coordinator = Coordinator::new();
coordinator.initialize(num_cpus::get());

// Access specific reactor
let external_reactor = Coordinator::reactor(0);
external_reactor.spawn_external(async {
    // Work on reactor 0
});

coordinator.join_all().unwrap();
```

## Cooperative I/O

### File Operations

```rust
use starfish_reactor::cooperative_io::file_open_options::FileOpenOptions;
use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;

// Read from file
let mut file = FileOpenOptions::new()
    .read(true)
    .open("input.txt")?;

let mut buffer = vec![0u8; 4096];
let n = file.read(&mut buffer).await?;

// Write to file
let mut file = FileOpenOptions::new()
    .write(true)
    .create(true)
    .open("output.txt")?;

file.write_all(b"Hello, World!").await?;
```

### TCP Networking

```rust
use starfish_reactor::cooperative_io::tcp_stream::TcpStream;
use starfish_reactor::cooperative_io::tcp_listener::TcpListener;
use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;

// TCP Client
let mut stream = TcpStream::connect("127.0.0.1:8080".parse().unwrap()).await?;
stream.write_all(b"Hello").await?;

let mut buf = vec![0u8; 1024];
let n = stream.read(&mut buf).await?;

// TCP Server
let listener = TcpListener::bind("127.0.0.1:8080".parse().unwrap()).await?;
let (mut client, addr) = listener.accept().await?;
println!("Connection from {}", addr);
```

### UDP Networking

```rust
use starfish_reactor::cooperative_io::udp_socket::UdpSocket;

let socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).await?;

// Send datagram
socket.send_to(b"Hello", "127.0.0.1:8080".parse().unwrap()).await?;

// Receive datagram
let mut buf = vec![0u8; 1024];
let (n, addr) = socket.recv_from(&mut buf).await?;
```

### I/O Timeouts

```rust
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;
use std::time::Duration;

let timeout = Some(IOTimeout::new(Duration::from_secs(5)));

// Read with timeout
let result = file.read_with_timeout(&mut buffer, &timeout).await;

// Connect with timeout
let stream = TcpStream::connect_with_timeout(addr, &timeout).await?;
```

## Cooperative Synchronization

### Events

```rust
use starfish_reactor::reactor::Reactor;

let reactor = Reactor::local_instance();
let event = reactor.create_event();

// In one task: wait for event
reactor.spawn(async move {
    event.wait().await;
    println!("Event signaled!");
});

// In another task: signal event
event.set();
```

### Fair Locks

```rust
let reactor = Reactor::local_instance();
let lock = reactor.create_fair_lock(0i32);

reactor.spawn(async move {
    let mut guard = lock.lock().await;
    *guard += 1;
    // Lock automatically released when guard is dropped
});
```

### Sleep/Delay

```rust
use starfish_reactor::cooperative_synchronization::delayed_future::cooperative_sleep;
use std::time::Duration;

// Sleep for 100ms
cooperative_sleep(Duration::from_millis(100)).await;
```

### Yielding

```rust
use starfish_reactor::reactor::cooperative_yield;

// Yield control to other tasks
cooperative_yield().await;
```

## Async Traits

### AsyncRead

```rust
pub trait AsyncRead {
    fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<usize>>;
}

// Extension methods: read(), read_exact(), read_exact_with_timeout()
```

### AsyncWrite

```rust
pub trait AsyncWrite {
    fn write_with_timeout(
        &mut self,
        buf: &[u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<usize>>;
}

// Extension methods: write(), write_all(), write_all_with_timeout()
```

## Custom I/O Manager

You can provide custom I/O manager options:

```rust
use starfish_reactor::coordinator::Coordinator;
use starfish_reactor::cooperative_io::io_manager::IOManagerCreateOptions;

// Use default IO manager with custom options
let mut coordinator = Coordinator::new();
coordinator.initialize_with_io_manager(
    DefaultIOManagerCreateOptions::new(),
    4
);
```

## Dependencies

- `starfish-core` - Core data structures (Treap for timeout management)
- `crossbeam` - Lock-free queue for external futures
- `socket2` - Low-level socket operations
- Platform-specific:
  - Linux: `io-uring`, `libc`
  - macOS: `kqueue-sys`, `libc`
  - Windows: `windows` crate

## Benchmarks

Run benchmarks:

```bash
cargo bench --bench reactor_benchmark
```

## Related Crates

| Crate | Description |
|-------|-------------|
| `starfish-core` | Core concurrent data structures |
| `starfish-epoch` | Epoch-based memory reclamation |
| `starfish-tls` | TLS/SSL support |
| `starfish-http` | HTTP protocol support |

## License

See the repository root for license information.
