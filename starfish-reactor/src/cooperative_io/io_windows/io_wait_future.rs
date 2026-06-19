//! Future type for awaiting Windows IOCP async I/O completion.
//!
//! Provides `IOWaitFuture`, which wraps a Windows `OVERLAPPED` structure and yields
//! to the reactor until the associated I/O operation completes on the completion port.
//!
//! `OVERLAPPED.hEvent` doubles as the completion-packet routing slot: it must
//! be 0 while the overlapped I/O call is issued — Windows captures the field
//! at submission, and a set low bit at that moment would suppress the
//! completion-port notification entirely. The kernel never reads or writes
//! the field after submission, so once the I/O call has returned,
//! `CompletionPortIOManager::register_io_wait` stores the owning
//! `IOWaitFuture`'s address in it for the dequeue path to read back. All
//! post-submission accesses happen on the reactor thread — see the manager's
//! module docs (invariant 1) for the full ordering argument.

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
/// the caller's stack.
///
/// hEvent in the OVERLAPPED is NOT set before the I/O call (setting the
/// low bit would suppress IOCP notifications). It stays 0 during the I/O
/// call and receives this future's address in `register_io_wait` (module
/// docs above).
///
/// # Usage Pattern
/// 1. Create OVERLAPPED on stack with default values
/// 2. Create IOWaitFuture with pointer to OVERLAPPED
/// 3. Start I/O operation (ReadFile, WriteFile, ConnectEx, etc.)
/// 4. Pin and await the IOWaitFuture
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
            timeout: timeout.map(IOTimeout::normalize),
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
        if self.io_result.is_some() {
            return true;
        }

        // HasOverlappedIoCompleted checks Internal field of OVERLAPPED.
        // If the value is -1 (STATUS_PENDING), the operation is not complete.
        //
        // SAFETY: self.overlapped is valid for the lifetime of this IOWaitFuture
        // (both live in the same async fn frame, OVERLAPPED declared first).
        let is_completed = unsafe { (*self.overlapped).Internal != STATUS_PENDING.0 as usize };

        if is_completed {
            let mut transferred = 0;
            // SAFETY: handle and overlapped are valid; `false` means don't block.
            let overlapped_result = unsafe {
                GetOverlappedResult(self.handle, self.overlapped, &mut transferred, false)
            };

            self.io_result = match overlapped_result {
                Ok(_) => Some(Ok(transferred as usize)),
                Err(err) => Some(Err(windows_error_to_io_error(err))),
            }
        }

        is_completed
    }

    pub fn timeout(&self) -> &Option<IOTimeout> {
        &self.timeout
    }
}

/// Convert a `windows::core::Error` from `GetOverlappedResult` to `io::Error`
/// with the correct `ErrorKind`.
///
/// The `windows` crate stores errors as HRESULTs.  `From<windows::core::Error>
/// for io::Error` passes the raw HRESULT i32 to `io::Error::from_raw_os_error`,
/// but Rust's `ErrorKind` mapping expects Win32 error codes, so the kinds come
/// out wrong (e.g. `ERROR_OPERATION_ABORTED` → `Other` instead of `TimedOut`).
///
/// This function extracts the Win32 code from Facility-Win32 HRESULTs
/// (`0x8007xxxx`) and additionally maps `ERROR_OPERATION_ABORTED` (995) to
/// `ErrorKind::TimedOut`, since the reactor's timeout handler is the only
/// code path that calls `CancelIoEx`.
fn windows_error_to_io_error(err: windows::core::Error) -> io::Error {
    const FACILITY_WIN32: u32 = 0x8007_0000;
    const ERROR_OPERATION_ABORTED: i32 = 995;

    let hresult = err.code().0 as u32;
    let win32_code = if hresult & 0xFFFF_0000 == FACILITY_WIN32 {
        (hresult & 0xFFFF) as i32
    } else {
        return err.into();
    };

    if win32_code == ERROR_OPERATION_ABORTED {
        io::Error::from(io::ErrorKind::TimedOut)
    } else {
        io::Error::from_raw_os_error(win32_code)
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
