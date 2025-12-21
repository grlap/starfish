use std::future::Future;
use std::io;
use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll};

use kqueue_sys::kevent;

use crate::cooperative_io::io_timeout::IOTimeout;
use crate::reactor::{FutureRuntime, Reactor, ScheduleReason};

pub(crate) struct IOCallbackWrapper<F>
where
    F: Fn(&kevent, &mut [u8]) -> io::Result<usize>,
{
    pub(crate) callback: F,
}

// Type alias for readability.
//
type IOCallback = Box<dyn Fn(&kevent, &mut [u8]) -> io::Result<usize>>;

// Implements IOWaitFuture.
//
pub(crate) struct IOWaitFuture {
    pub(crate) k_event: kevent,
    future_runtime: Option<FutureRuntime>,
    buffer: *mut [u8],
    should_continue: bool,
    pub(crate) buffer_offset: usize,
    io_result: Option<io::Result<usize>>,
    timeout: Option<IOTimeout>,
    io_callback: IOCallback,
}

impl IOWaitFuture {
    pub fn new(
        k_event: kevent,
        buffer: *const [u8],
        should_continue: bool,
        timeout: &Option<IOTimeout>,
        io_callback: IOCallbackWrapper<impl Fn(&kevent, &mut [u8]) -> io::Result<usize> + 'static>,
    ) -> Self {
        IOWaitFuture {
            k_event,
            future_runtime: None,
            buffer: buffer as *mut [u8],
            buffer_offset: 0,
            should_continue,
            io_result: None,
            timeout: *timeout,
            io_callback: Box::new(io_callback.callback),
        }
    }

    pub fn set_io_result(&mut self, io_result: io::Result<usize>) {
        self.io_result = Some(io_result)
    }

    pub fn set_future_runtime(&mut self, future_runtime: FutureRuntime) {
        self.future_runtime = Some(future_runtime);
    }

    pub fn take_future_runtime(&mut self) -> Option<FutureRuntime> {
        self.future_runtime.take()
    }

    pub fn is_completed(&mut self) -> bool {
        self.io_result.is_some()
    }

    pub fn timeout(&self) -> &Option<IOTimeout> {
        &self.timeout
    }

    pub(crate) fn set_timeout(&mut self, timeout: Option<IOTimeout>) {
        self.timeout = timeout;
    }

    /// Handles IO kqueue event.
    ///
    pub fn handle_kevent(&mut self) {
        let buffer: &mut [u8] = &mut (unsafe { &mut *self.buffer })[self.buffer_offset..];

        let io_result = (self.io_callback)(&self.k_event, buffer);

        loop {
            match io_result {
                Ok(length) => {
                    self.buffer_offset += length;

                    if !self.should_continue || self.buffer_offset == self.buffer.len() {
                        self.set_io_result(Ok(self.buffer_offset));
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    // No more datagrams available, break and wait for next event
                    break;
                }
                Err(err) => {
                    self.set_io_result(Err(err));
                    break;
                }
            }
        }
    }
}

impl Future for IOWaitFuture {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        match self.io_result.take() {
            Some(io_result) => Poll::Ready(io_result),
            None => {
                // Get the reactor from the context.
                //
                let reactor = Reactor::local_instance();

                let io_wait_future_ptr: *const IOWaitFuture = ptr::addr_of!(*self);

                // Update the scheduler with rescheduling reason.
                //
                reactor.set_schedule_reason(ScheduleReason::IOWait(io_wait_future_ptr));

                Poll::Pending
            }
        }
    }
}
