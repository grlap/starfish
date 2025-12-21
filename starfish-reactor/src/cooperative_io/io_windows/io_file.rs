use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::pin::Pin;

use windows::Win32::Foundation::{ERROR_IO_PENDING, HANDLE};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, ReadFile, WriteFile};
use windows::Win32::System::IO::OVERLAPPED;
use windows::core::HRESULT;

use crate::cooperative_io::file_open_options::{AsyncOpenOptions, FileOpenOptions};
use crate::cooperative_io::io_timeout::IOTimeout;
use crate::reactor::Reactor;

use super::completion_port_io_manager::CompletionPortIOManager;
use super::io_wait_future::IOWaitFuture;

impl AsyncOpenOptions for OpenOptions {
    fn as_async_io(&mut self) -> FileOpenOptions<'_> {
        FileOpenOptions::new(self.attributes(FILE_FLAG_OVERLAPPED.0))
    }
}

/// Registers a File Handle with IOManager.
///
pub(crate) fn prepare_io_with_file(file: &File) -> Result<(), io::Error> {
    Reactor::local_instance()
        .io_manager::<CompletionPortIOManager>()
        .map_or(Ok(()), |io_manager| {
            io_manager.prepare_io(HANDLE(file.as_raw_handle() as isize))
        })
}

/// Reads data from the specified file.
/// Returns number of bytes read.
///
pub async fn read_file(
    file: &mut File,
    buffer: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    // Prepare OVERLAPPED structure.
    //
    let mut overlapped = OVERLAPPED::default();

    let result = unsafe {
        ReadFile(
            HANDLE(file.as_raw_handle() as isize),
            Some(buffer),
            None,
            Some(&mut overlapped),
        )
    };

    match result {
        Ok(_) => return Ok(overlapped.InternalHigh),
        Err(error) => {
            // Check if IO is pending or operation failed.
            //
            if error.code() != HRESULT::from_win32(ERROR_IO_PENDING.0) {
                return Err(error.into());
            }
        }
    };

    // SAFETY: `overlapped` is a stack local that won't be moved before the await completes.
    // Pin documents that kernel holds pointer to this structure during async I/O.
    //
    let mut overlapped_pinned: Pin<&mut OVERLAPPED> =
        unsafe { Pin::new_unchecked(&mut overlapped) };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(file.as_raw_handle() as isize),
        &mut *overlapped_pinned,
        timeout,
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}

/// Writes data from the specified file.
/// Returns number of bytes written.
///
pub async fn write_file(
    file: &mut File,
    buffer: &[u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    // Prepare OVERLAPPED structure.
    //
    let mut overlapped = OVERLAPPED::default();

    let result = unsafe {
        WriteFile(
            HANDLE(file.as_raw_handle() as isize),
            Some(buffer),
            None,
            Some(&mut overlapped),
        )
    };

    match result {
        Ok(_) => return Ok(overlapped.InternalHigh),
        Err(error) => {
            // Check if IO is pending or operation failed.
            //
            if error.code() != HRESULT::from_win32(ERROR_IO_PENDING.0) {
                return Err(error.into());
            }
        }
    };

    // SAFETY: `overlapped` is a stack local that won't be moved before the await completes.
    // Pin documents that kernel holds pointer to this structure during async I/O.
    //
    let mut overlapped_pinned: Pin<&mut OVERLAPPED> =
        unsafe { Pin::new_unchecked(&mut overlapped) };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(file.as_raw_handle() as isize),
        &mut *overlapped_pinned,
        timeout,
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}
