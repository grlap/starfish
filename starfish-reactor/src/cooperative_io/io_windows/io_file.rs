//! Windows file I/O via OVERLAPPED operations.
//!
//! Implements async file read and write using Windows OVERLAPPED I/O, opening files
//! with `FILE_FLAG_OVERLAPPED` for integration with the IOCP-based I/O manager.
//!
//! Overlapped handles IGNORE the kernel file pointer — every operation works
//! at the explicit `OVERLAPPED.Offset`/`OffsetHigh`, and there is no "use and
//! advance the cursor" mode like io_uring's offset `-1`. The sequential
//! `read(2)`/`write(2)` contract (see `cooperative_io/file.rs`) is therefore
//! implemented deliberately: `FilePosition` tracks the cursor, each operation
//! stamps it into its `OVERLAPPED` and advances it by the bytes transferred
//! on success. Reads at/past EOF complete with `ERROR_HANDLE_EOF` rather than
//! a 0-byte transfer; both paths map it to `Ok(0)` for cross-platform parity.
//! (Append-mode handles would ignore the explicit offset for writes; nothing
//! in the workspace opens files in append mode.)

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::pin::Pin;
use std::ptr;

use windows::Win32::Foundation::{ERROR_HANDLE_EOF, ERROR_IO_PENDING, HANDLE};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, ReadFile, WriteFile};
use windows::Win32::System::IO::OVERLAPPED;
use windows::core::HRESULT;

use super::completion_port_io_manager::CompletionPortIOManager;
use super::io_wait_future::IOWaitFuture;
use crate::cooperative_io::file_open_options::{AsyncOpenOptions, FileOpenOptions};
use crate::cooperative_io::io_manager::local_io_manager;
use crate::cooperative_io::io_timeout::IOTimeout;

impl AsyncOpenOptions for OpenOptions {
    fn as_async_io(&mut self) -> FileOpenOptions<'_> {
        FileOpenOptions::new(self.attributes(FILE_FLAG_OVERLAPPED.0))
    }
}

/// Registers a File Handle with IOManager.
///
pub(crate) fn prepare_io_with_file(file: &File) -> Result<(), io::Error> {
    local_io_manager::<CompletionPortIOManager>().map_or(Ok(()), |io_manager| {
        io_manager.prepare_io(HANDLE(file.as_raw_handle() as isize))
    })
}

/// Explicit file cursor backing the sequential read/write contract:
/// overlapped handles ignore the kernel file pointer, so the position lives
/// here and is stamped into each operation's `OVERLAPPED` (module docs).
pub(crate) struct FilePosition {
    offset: u64,
}

impl FilePosition {
    pub(crate) fn new() -> Self {
        FilePosition { offset: 0 }
    }

    fn advance(&mut self, transferred: usize) -> io::Result<()> {
        self.offset = self
            .offset
            .checked_add(transferred as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file position overflow"))?;

        Ok(())
    }
}

/// Stamps the cursor into the OVERLAPPED before the I/O call — the only place
/// Windows accepts a file offset for overlapped operations.
fn set_overlapped_offset(overlapped: &mut OVERLAPPED, offset: u64) {
    overlapped.Anonymous.Anonymous.Offset = offset as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
}

/// Reads data from the specified file.
/// Returns number of bytes read.
///
pub(crate) async fn read_file(
    file: &mut File,
    position: &mut FilePosition,
    buffer: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    let handle = HANDLE(file.as_raw_handle() as isize);

    let mut overlapped = OVERLAPPED::default();
    set_overlapped_offset(&mut overlapped, position.offset);
    let overlapped_ptr = ptr::addr_of_mut!(overlapped);

    let mut io_wait_future = IOWaitFuture::new(handle, overlapped_ptr, timeout);

    let result = unsafe { ReadFile(handle, Some(buffer), None, Some(overlapped_ptr)) };

    // Don't return early on Ok — even a synchronously-completed call still
    // queues a completion packet to the port, and the dequeue path resolves
    // it through this frame's OVERLAPPED → IOWaitFuture (manager invariant 1).
    //
    if let Err(error) = result
        && error.code() != HRESULT::from_win32(ERROR_IO_PENDING.0)
    {
        // A synchronous ERROR_HANDLE_EOF means the read never started (no
        // completion packet will arrive): report it as Ok(0) — POSIX read(2)
        // semantics, matching the other backends (module docs).
        if error.code() == HRESULT::from_win32(ERROR_HANDLE_EOF.0) {
            return Ok(0);
        }

        return Err(error.into());
    }

    // SAFETY: `overlapped` and `io_wait_future` are state-machine locals
    // pinned by the async fn's Box allocation. They won't move before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let transferred = match pinned_io_wait_future.await {
        Ok(transferred) => transferred,
        // An overlapped read at/past EOF fails with ERROR_HANDLE_EOF instead
        // of transferring 0 bytes: map to Ok(0) (module docs).
        Err(error) if error.raw_os_error() == Some(ERROR_HANDLE_EOF.0 as i32) => 0,
        Err(error) => return Err(error),
    };
    position.advance(transferred)?;

    Ok(transferred)
}

/// Writes data from the specified file.
/// Returns number of bytes written.
///
pub(crate) async fn write_file(
    file: &mut File,
    position: &mut FilePosition,
    buffer: &[u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    let handle = HANDLE(file.as_raw_handle() as isize);

    let mut overlapped = OVERLAPPED::default();
    set_overlapped_offset(&mut overlapped, position.offset);
    let overlapped_ptr = ptr::addr_of_mut!(overlapped);

    let mut io_wait_future = IOWaitFuture::new(handle, overlapped_ptr, timeout);

    let result = unsafe { WriteFile(handle, Some(buffer), None, Some(overlapped_ptr)) };

    // Don't return early on Ok — even a synchronously-completed call still
    // queues a completion packet to the port, and the dequeue path resolves
    // it through this frame's OVERLAPPED → IOWaitFuture (manager invariant 1).
    //
    if let Err(error) = result
        && error.code() != HRESULT::from_win32(ERROR_IO_PENDING.0)
    {
        return Err(error.into());
    }

    // SAFETY: `overlapped` and `io_wait_future` are state-machine locals
    // pinned by the async fn's Box allocation. They won't move before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let transferred = pinned_io_wait_future.await?;
    position.advance(transferred)?;

    Ok(transferred)
}
