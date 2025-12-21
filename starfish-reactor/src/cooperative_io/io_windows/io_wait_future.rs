use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{io, ptr};

use windows::Win32::Foundation::{HANDLE, STATUS_PENDING};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

use crate::cooperative_io::io_timeout::IOTimeout;
use crate::reactor::{FutureRuntime, Reactor, ScheduleReason};

/// IOWaitFuture implementation for Windows async I/O.
///
/// This struct manages the lifecycle of an async I/O operation on Windows.
/// It stores a pointer to an external OVERLAPPED structure that lives on
/// the caller's stack. The hEvent field in the external OVERLAPPED is set
/// to point to this IOWaitFuture (with low bit set) during registration,
/// allowing the IOCP handler to find the IOWaitFuture when I/O completes.
///
/// # Usage Pattern
/// 1. Create OVERLAPPED on stack with default values
/// 2. Start I/O operation (ReadFile, WriteFile, ConnectEx, etc.)
/// 3. If I/O returns ERROR_IO_PENDING, create IOWaitFuture with pointer to OVERLAPPED
/// 4. Pin and await the IOWaitFuture
///
/// The hEvent is set in `register_io_wait()` after I/O has started. There's a race
/// window where I/O could complete before hEvent is set - this is handled by the
/// completion port manager re-queuing such completions.
///
pub(crate) struct IOWaitFuture {
    pub(crate) handle: HANDLE,
    pub(crate) overlapped: *mut OVERLAPPED,
    future_runtime: Option<FutureRuntime>,
    io_result: Option<io::Result<usize>>,
    timeout: Option<IOTimeout>,
}

impl IOWaitFuture {
    /// Creates a new IOWaitFuture with an external OVERLAPPED.
    ///
    /// # Arguments
    /// * `handle` - The Windows handle for the I/O operation
    /// * `overlapped` - Pointer to the OVERLAPPED structure passed to the I/O function
    /// * `timeout` - Optional timeout for the operation
    ///
    /// # Safety
    /// The external overlapped must remain valid for the lifetime of this IOWaitFuture.
    /// This is typically ensured by keeping the overlapped on the stack of the async
    /// function that creates this IOWaitFuture.
    ///
    pub fn new(handle: HANDLE, overlapped: *mut OVERLAPPED, timeout: &Option<IOTimeout>) -> Self {
        IOWaitFuture {
            handle,
            overlapped,
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

    pub fn has_future_runtime(&self) -> bool {
        self.future_runtime.is_some()
    }

    pub fn take_future_runtime(&mut self) -> Option<FutureRuntime> {
        self.future_runtime.take()
    }

    pub fn is_completed(&mut self) -> bool {
        if self.io_result.is_some() {
            return true;
        }

        // HasOverlappedIoCompleted checks Internal field of OVERLAPPED.
        // If the value is -1 (STATUS_PENDING), the operation is not complete.
        //
        let is_completed = unsafe { (*self.overlapped).Internal != STATUS_PENDING.0 as usize };

        if is_completed {
            let mut transferred = 0;
            let overlapped_result = unsafe {
                GetOverlappedResult(self.handle, self.overlapped, &mut transferred, false)
            };

            self.io_result = match overlapped_result {
                Ok(_) => Some(Ok(transferred as usize)),
                Err(err) => Some(Err(err.into())),
            }
        }

        is_completed
    }

    pub fn timeout(&self) -> &Option<IOTimeout> {
        &self.timeout
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
