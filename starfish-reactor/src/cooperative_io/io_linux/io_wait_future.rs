use std::future::Future;
use std::io;
use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll};

use io_uring::squeue;

use crate::cooperative_io::io_timeout::IOTimeout;
use crate::reactor::{FutureRuntime, Reactor, ScheduleReason};

// Implements IOWaitFuture.
//
pub(crate) struct IOWaitFuture {
    pub(crate) queue_entry: Option<squeue::Entry>,
    future_runtime: Option<FutureRuntime>,
    io_result: Option<io::Result<usize>>,
    timeout: Option<IOTimeout>,
}

impl IOWaitFuture {
    pub fn new(queue_entry: squeue::Entry, timeout: &Option<IOTimeout>) -> Self {
        IOWaitFuture {
            queue_entry: Some(queue_entry),
            future_runtime: None,
            io_result: None,
            timeout: *timeout,
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
