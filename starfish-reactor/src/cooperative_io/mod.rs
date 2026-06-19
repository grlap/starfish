//! Async I/O abstractions and platform-specific backends.
//!
//! Re-exports cross-platform traits (`AsyncRead`, `AsyncWrite`, `IOManager`) and
//! concrete types (`File`, `TcpListener`, `TcpStream`, `UdpSocket`), along with
//! the platform-appropriate I/O backend (io_uring, kqueue, or IOCP).

pub mod async_read;
pub mod async_write;
pub mod file;
pub mod file_open_options;
pub mod io_manager;
pub mod io_timeout;
pub mod tcp_listener;
pub mod tcp_stream;
pub mod udp_socket;

// Windows
//
#[cfg(windows)]
pub mod io_windows;
#[cfg(windows)]
pub use self::io_windows::*;

// MacOS
//
#[cfg(target_os = "macos")]
pub mod io_macos;
#[cfg(target_os = "macos")]
pub use self::io_macos::*;

// Linux
//
#[cfg(target_os = "linux")]
pub mod io_linux;
#[cfg(target_os = "linux")]
pub use self::io_linux::*;
