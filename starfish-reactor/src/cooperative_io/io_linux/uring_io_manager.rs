use std::any;
use std::io;
use std::time::{Duration, UNIX_EPOCH};

use io_uring::IoUring;
use io_uring::opcode;
use io_uring::types;
use libc::EBUSY;

use crate::cooperative_io::io_manager::{IOManager, IOManagerCreateOptions, IOManagerHolder};
use crate::reactor::FutureRuntime;

use super::io_wait_future::IOWaitFuture;

pub(crate) struct UringIOManager {
    io_uring: IoUring,
    active_io: i32,
}

/// [`UringIOManager`].
/// See [`IOManager`].
///
const DEFAULT_QUEUE_SIZE: u32 = 1024;

impl UringIOManager {
    pub(crate) fn try_new(queue_size: u32) -> io::Result<Self> {
        let io_uring = IoUring::new(queue_size)?;

        Ok(UringIOManager {
            io_uring,
            active_io: 0,
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

        let completed_entry = self.io_uring.completion().next();
        let result: Option<(FutureRuntime, *const IOWaitFuture)> = match completed_entry {
            Some(queue_entry) => {
                let io_wait_future_ptr = queue_entry.user_data() as *mut IOWaitFuture;

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

                    self.active_io -= 1;

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

                let result = unsafe { self.io_uring.submission().push(&queue_entry) };

                if result.is_err() {
                    return Err(io::Error::from_raw_os_error(EBUSY));
                }

                // Create a Timespec entry for timeout.
                //
                let duration = timeout
                    .cancel_at_time()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::from_secs(0));

                let ts = types::Timespec::new()
                    .sec(duration.as_secs())
                    .nsec(duration.subsec_nanos());

                let timeout_op = opcode::Timeout::new(&ts).build();

                let result = unsafe { self.io_uring.submission().push(&timeout_op) };

                if result.is_err() {
                    return Err(io::Error::from_raw_os_error(EBUSY));
                }

                // Reset future timeout as it is no longer needed.
                // Reactor will not track the timeout.
                //
                io_wait_future.set_timeout(None);
            }
            None => {
                // Not timeout specified, enqueue request.
                //
                let result = unsafe { self.io_uring.submission().push(&queue_entry) };

                if result.is_err() {
                    return Err(io::Error::from_raw_os_error(EBUSY));
                }
            }
        }

        // Submit request.
        //
        let result = self.io_uring.submit();

        match result {
            Ok(_) => {
                self.active_io += 1;
                Ok(())
            }
            Err(_) => Err(io::Error::from_raw_os_error(EBUSY)),
        }
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

/// Configuration options for creating a [`UringIOManager`].
pub struct UringIOManagerCreateOptions {
    queue_size: u32,
}

impl UringIOManagerCreateOptions {
    pub fn new() -> Self {
        UringIOManagerCreateOptions {
            queue_size: DEFAULT_QUEUE_SIZE,
        }
    }

    /// Sets the io_uring submission/completion queue size.
    /// Must be a power of 2. Default is 1024.
    pub fn with_queue_size(mut self, queue_size: u32) -> Self {
        self.queue_size = queue_size;
        self
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
            io_manager: Box::new(UringIOManager::try_new(self.queue_size)?),
        })
    }
}
