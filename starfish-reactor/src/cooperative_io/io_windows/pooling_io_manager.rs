use std::{cell::RefCell, collections::VecDeque, io};

use windows::Win32::System::IO::CancelIoEx;

use crate::{
    cooperative_io::io_manager::{IOManager, IOManagerCreateOptions, IOManagerHolder},
    reactor::FutureRuntime,
};

use super::io_wait_future::IOWaitFuture;

/// [`PoolingIOManager`] maintains a collection of pending I/O futures and keeps track
/// of the number of active I/O operations. It provides methods to register new I/O
/// operations, check for completed operations, and determine if any operations are
/// currently active.
///
/// See [`IOManager`].
///
pub(crate) struct PoolingIOManager {
    pending_io_futures: RefCell<VecDeque<*const IOWaitFuture>>,
    active_io: i32,
}

impl PoolingIOManager {
    /// Creates a new instance of PoolingIOManager.
    ///
    pub fn new() -> Self {
        PoolingIOManager {
            pending_io_futures: RefCell::new(VecDeque::new()),
            active_io: 0,
        }
    }

    pub fn try_new() -> io::Result<Self> {
        Ok(Self::new())
    }
}

impl IOManager for PoolingIOManager {
    /// Check if there are active I/O operations.
    ///
    fn has_active_io(&self) -> bool {
        self.active_io != 0
    }

    /// See [`IOManager::completed_io`].
    ///
    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)> {
        let mut pending_io_futures = self.pending_io_futures.borrow_mut();

        if !pending_io_futures.is_empty() {
            // Check pending IO.
            //
            for (index, io_wait_future_ptr) in pending_io_futures.iter().enumerate() {
                let io_wait_future = unsafe { &mut *{ *io_wait_future_ptr as *mut IOWaitFuture } };

                if io_wait_future.is_completed() {
                    let completed_io_wait_future_ptr =
                        pending_io_futures.swap_remove_front(index).unwrap();
                    let completed_io_wait_future =
                        unsafe { &mut *{ completed_io_wait_future_ptr as *mut IOWaitFuture } };
                    let future_runtime = completed_io_wait_future.take_future_runtime().unwrap();

                    self.active_io -= 1;

                    return Some((future_runtime, completed_io_wait_future_ptr));
                }
            }
        }

        None
    }

    /// See [`IOManager::register_io_wait`].
    ///
    fn register_io_wait(
        &mut self,
        io_wait_future_ptr: *const IOWaitFuture,
    ) -> Result<(), io::Error> {
        self.pending_io_futures
            .borrow_mut()
            .push_back(io_wait_future_ptr);

        self.active_io += 1;

        Ok(())
    }

    /// See [`IOManager::cancel_io_wait`].
    ///
    fn cancel_io_wait(&mut self, io_wait_future_ptr: *const IOWaitFuture) -> Result<(), io::Error> {
        let io_future_wait = unsafe { &*io_wait_future_ptr };

        unsafe { CancelIoEx(io_future_wait.handle, Some(io_future_wait.overlapped)) }?;

        Ok(())
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any
    where
        Self: 'static,
    {
        self
    }
}

/// Implements IOManager create options.
///
/// See [`IOManagerCreateOptions`].
///
pub struct PoolingIOManagerCreateOptions {}

impl PoolingIOManagerCreateOptions {
    pub fn new() -> Self {
        PoolingIOManagerCreateOptions {}
    }
}

impl Default for PoolingIOManagerCreateOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl IOManagerCreateOptions for PoolingIOManagerCreateOptions {
    fn try_new_io_manager(&self) -> io::Result<IOManagerHolder> {
        Ok(IOManagerHolder {
            io_manager: Box::new(PoolingIOManager::new()),
        })
    }
}
