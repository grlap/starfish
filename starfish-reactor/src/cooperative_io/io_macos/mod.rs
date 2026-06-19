//! macOS I/O backend using kqueue.
//!
//! Re-exports `KqueueIOManager` as `DefaultIOManager` and provides modules for
//! kqueue-based file and network operations.

use kqueue_io_manager::{KqueueIOManager, KqueueIOManagerReactorCreateOptions};

pub mod io_file;
pub mod io_network;
pub mod io_wait_future;
pub mod kqueue_io_manager;

pub(crate) type DefaultIOManager = KqueueIOManager;
pub(crate) type DefaultIOManagerCreateOptions = KqueueIOManagerReactorCreateOptions;
