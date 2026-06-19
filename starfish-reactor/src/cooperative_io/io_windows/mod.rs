//! Windows I/O backend using I/O Completion Ports (IOCP).
//!
//! Re-exports `CompletionPortIOManager` as `DefaultIOManager` and provides
//! modules for IOCP-based file, network, and completion port operations.

use completion_port_io_manager::{CompletionPortIOManager, CompletionPortIOManagerCreateOptions};

pub mod completion_port_io_manager;
pub mod io_file;
pub mod io_network;
pub mod io_wait_future;
pub mod iocp;
pub mod pooling_io_manager;

pub(crate) type DefaultIOManager = CompletionPortIOManager;
pub(crate) type DefaultIOManagerCreateOptions = CompletionPortIOManagerCreateOptions;
