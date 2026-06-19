//! Linux I/O backend using io_uring.
//!
//! Re-exports `UringIOManager` as `DefaultIOManager` and provides modules for
//! io_uring-based file and network operations.

use uring_io_manager::{UringIOManager, UringIOManagerCreateOptions};

pub mod io_file;
pub mod io_network;
pub mod io_wait_future;
pub mod uring_io_manager;

pub(crate) type DefaultIOManager = UringIOManager;
pub(crate) type DefaultIOManagerCreateOptions = UringIOManagerCreateOptions;
