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

pub async fn read_file(
    file: &mut File,
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
        .build(),
        timeout,
    );

    let pinned_io_wait_future = unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}

pub async fn write_file(
    file: &mut File,
    buf: &[u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        opcode::Write::new(types::Fd(file.as_raw_fd()), buf.as_ptr(), buf.len() as u32).build(),
        timeout,
    );

    let pinned_io_wait_future = unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}
