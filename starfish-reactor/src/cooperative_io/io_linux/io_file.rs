//! Linux file I/O via io_uring opcodes.
//!
//! Implements async file read and write by submitting io_uring `Read` and `Write`
//! opcodes, returning `IOWaitFuture` instances that complete when the kernel signals.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::pin::Pin;

use io_uring::{opcode, types};

use crate::cooperative_io::file_open_options::{AsyncOpenOptions, FileOpenOptions};
use crate::cooperative_io::io_timeout::IOTimeout;

use super::io_wait_future::IOWaitFuture;

impl AsyncOpenOptions for OpenOptions {
    fn as_async_io(&mut self) -> FileOpenOptions<'_> {
        FileOpenOptions::new(self)
    }
}

/// Registers a File Handle with IOManager.
///
#[inline]
pub(crate) fn prepare_io_with_file(_: &File) -> Result<(), io::Error> {
    // Noop for URingIOManger.
    //
    Ok(())
}

/// Zero-sized stub of the file-position contract (`cooperative_io/file.rs`):
/// the io_uring backend reads/writes at offset `-1` — the kernel file
/// position — so no userspace cursor is needed (see `USE_FILE_POSITION`).
pub(crate) struct FilePosition;

impl FilePosition {
    pub(crate) fn new() -> Self {
        FilePosition
    }
}

/// io_uring `Read`/`Write` interpret the SQE offset as an ABSOLUTE byte
/// offset, and the io-uring crate defaults it to 0 — NOT the file cursor.
/// Passing `-1` (`u64::MAX`) selects "use and advance the file position"
/// (`read(2)`/`write(2)` semantics), which repeated reads/writes on the
/// same `File` (e.g. `read_exact`/`write_all` loops) rely on. Without it,
/// every operation targets byte 0: multi-chunk writes keep only the last
/// chunk and multi-chunk reads never advance.
const USE_FILE_POSITION: u64 = u64::MAX;

pub(crate) async fn read_file(
    file: &mut File,
    _: &mut FilePosition,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        opcode::Read::new(
            types::Fd(file.as_raw_fd()),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
        .offset(USE_FILE_POSITION)
        .build(),
        timeout,
    );

    let pinned_io_wait_future = unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}

pub(crate) async fn write_file(
    file: &mut File,
    _: &mut FilePosition,
    buf: &[u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        opcode::Write::new(types::Fd(file.as_raw_fd()), buf.as_ptr(), buf.len() as u32)
            .offset(USE_FILE_POSITION)
            .build(),
        timeout,
    );

    let pinned_io_wait_future = unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}
