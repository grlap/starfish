use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::pin::Pin;
use std::ptr;

use kqueue_sys::EventFilter;
use kqueue_sys::EventFlag;
use kqueue_sys::FilterFlag;
use kqueue_sys::kevent;

use crate::cooperative_io::file_open_options::{AsyncOpenOptions, FileOpenOptions};
use crate::cooperative_io::io_timeout::IOTimeout;

use super::io_wait_future::{IOCallbackWrapper, IOWaitFuture};

impl AsyncOpenOptions for OpenOptions {
    fn as_async_io(&mut self) -> FileOpenOptions<'_> {
        FileOpenOptions::new(self)
    }
}

/// Registers a File Handle with IOManager.
///
#[inline]
pub(crate) fn prepare_io_with_file(_: &File) -> Result<(), io::Error> {
    // Noop for KQueueIOManger.
    //
    Ok(())
}

pub async fn read_file(
    file: &mut File,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    let k_event = kevent {
        ident: file.as_raw_fd() as usize,
        filter: kqueue_sys::EventFilter::EVFILT_READ,
        fflags: FilterFlag::empty(),
        data: 0,
        udata: ptr::null_mut(),
        flags: EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR,
    };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        k_event,
        buf,
        true,
        timeout,
        IOCallbackWrapper {
            callback: move |k_event: &kevent, buf: &mut [u8]| -> io::Result<usize> {
                assert_eq!(EventFilter::EVFILT_READ, k_event.filter);

                let mut file = unsafe { File::from_raw_fd(k_event.ident as i32) };

                let result = file.read(buf);
                let _ = file.into_raw_fd();

                result
            },
        },
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    // Pin documents that IOManager stores a pointer to this future.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}

pub async fn write_file(
    file: &mut File,
    buf: &[u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    let k_event = kevent {
        ident: file.as_raw_fd() as usize,
        filter: kqueue_sys::EventFilter::EVFILT_WRITE,
        fflags: FilterFlag::empty(),
        data: 0,
        udata: ptr::null_mut(),
        flags: EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR,
    };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        k_event,
        buf,
        true,
        timeout,
        IOCallbackWrapper {
            callback: move |k_event: &kevent, buf: &mut [u8]| -> io::Result<usize> {
                assert_eq!(EventFilter::EVFILT_WRITE, k_event.filter);

                let mut file = unsafe { File::from_raw_fd(k_event.ident as i32) };

                let result = file.write(buf);
                let _ = file.into_raw_fd();

                result
            },
        },
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    // Pin documents that IOManager stores a pointer to this future.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}
