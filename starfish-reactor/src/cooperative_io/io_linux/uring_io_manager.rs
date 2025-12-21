use std::any;
use std::io;
use std::time::{Duration, UNIX_EPOCH};

use io_uring::IoUring;
use io_uring::opcode;
use io_uring::types;

use crate::cooperative_io::io_manager::{IOManager, IOManagerCreateOptions, IOManagerHolder};
use crate::reactor::FutureRuntime;

use super::io_wait_future::IOWaitFuture;

pub(crate) struct UringIOManager {
    io_uring: IoUring,
    active_io: i32,
    pending_submit: bool,
}

/// [`UringIOManager`].
/// See [`IOManager`].
///
impl UringIOManager {
    pub(crate) fn try_new() -> io::Result<Self> {
        let io_uring = IoUring::new(1024)?;

        Ok(UringIOManager {
            io_uring,
            active_io: 0,
            pending_submit: false,
        })
    }
}

impl IOManager for UringIOManager {
    fn has_active_io(&self) -> bool {
        self.active_io != 0
    }

    /// See [`IOManager::completed_io`].
    ///
    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)> {
        // Check if there are any submitted entries.
        //
        if self.active_io == 0 {
            return None;
        }

        // Retry any pending submit from previous failed attempt.
        //
        if self.pending_submit && self.io_uring.submit().is_ok() {
            self.pending_submit = false;
        }

        let completed_entry = self.io_uring.completion().next();
        let result: Option<(FutureRuntime, *const IOWaitFuture)> = match completed_entry {
            Some(queue_entry) => {
                self.active_io -= 1;

                let io_wait_future_ptr = queue_entry.user_data() as *mut IOWaitFuture;

                // Null user_data indicates a linked timeout completion - already decremented active_io.
                //
                if io_wait_future_ptr.is_null() {
                    None
                } else {
                    let completed_io_wait_future = unsafe { &mut *io_wait_future_ptr };

                    let result = queue_entry.result();
                    if result >= 0 {
                        completed_io_wait_future.set_io_result(Ok(result as usize));
                    } else {
                        completed_io_wait_future
                            .set_io_result(Err(io::Error::from_raw_os_error(-result)));
                    }

                    let future_runtime = completed_io_wait_future.take_future_runtime().unwrap();

                    Some((future_runtime, io_wait_future_ptr))
                }
            }
            None => None,
        };

        result
    }

    /// See [`IOManager::register_io_wait`].
    ///
    fn register_io_wait(
        &mut self,
        io_wait_future_ptr: *const super::io_wait_future::IOWaitFuture,
    ) -> Result<(), io::Error> {
        let io_wait_future = unsafe { &mut *{ io_wait_future_ptr as *mut IOWaitFuture } };

        let mut queue_entry = io_wait_future.queue_entry.take().unwrap();
        queue_entry = queue_entry.user_data(io_wait_future_ptr as u64);

        match io_wait_future.timeout() {
            Some(timeout) => {
                // Link the entry with the next operation.
                //
                queue_entry = queue_entry.flags(io_uring::squeue::Flags::IO_LINK);

                unsafe { self.io_uring.submission().push(&queue_entry) }
                    .map_err(|_| io::Error::from_raw_os_error(libc::EBUSY))?;

                self.active_io += 1;

                // Create a Timespec entry for linked timeout.
                //
                let duration = timeout
                    .cancel_at_time()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::from_secs(0));

                let ts = types::Timespec::new()
                    .sec(duration.as_secs())
                    .nsec(duration.subsec_nanos());

                // Use LinkTimeout for linked operations, with null user_data.
                //
                let timeout_op = opcode::LinkTimeout::new(&ts).build().user_data(0);

                unsafe { self.io_uring.submission().push(&timeout_op) }
                    .map_err(|_| io::Error::from_raw_os_error(libc::EBUSY))?;

                self.active_io += 1;

                // Reset future timeout as it is no longer needed.
                // Reactor will not track the timeout.
                //
                io_wait_future.set_timeout(None);
            }
            None => {
                // No timeout specified, enqueue request.
                //
                unsafe { self.io_uring.submission().push(&queue_entry) }
                    .map_err(|_| io::Error::from_raw_os_error(libc::EBUSY))?;

                self.active_io += 1;
            }
        };

        // Submit request. If submit fails, defer to next completed_io call.
        // We must return Ok to keep the pinned IOWaitFuture alive.
        //
        if self.io_uring.submit().is_err() {
            self.pending_submit = true;
        }

        Ok(())
    }

    /// See [`IOManager::cancel_io_wait`].
    ///
    fn cancel_io_wait(&mut self, _: *const IOWaitFuture) -> Result<(), io::Error> {
        Ok(())
    }

    fn as_any(&mut self) -> &mut dyn any::Any
    where
        Self: 'static,
    {
        self
    }
}

// Implements IOManager create options.
//
pub struct UringIOManagerCreateOptions {}

impl UringIOManagerCreateOptions {
    pub fn new() -> Self {
        UringIOManagerCreateOptions {}
    }
}

impl Default for UringIOManagerCreateOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl IOManagerCreateOptions for UringIOManagerCreateOptions {
    fn try_new_io_manager(&self) -> io::Result<IOManagerHolder> {
        Ok(IOManagerHolder {
            io_manager: Box::new(UringIOManager::try_new()?),
        })
    }
}
