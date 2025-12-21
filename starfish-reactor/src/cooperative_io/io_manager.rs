use std::any;
use std::io;

use crate::reactor::FutureRuntime;

use super::io_wait_future::IOWaitFuture;

/// IO manager handles pooling and tracking of I/O operations.
///
pub(crate) trait IOManager: any::Any {
    fn has_active_io(&self) -> bool;

    /// Returns FutureRuntime associated with next completed IO Future if available.
    ///
    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)>;

    /// Registers IOWaitFuture. Future will wait until pending IO is completed.
    ///
    fn register_io_wait(
        &mut self,
        io_wait_future_ptr: *const IOWaitFuture,
    ) -> Result<(), io::Error>;

    // Cancels an outstanding IO.
    //
    fn cancel_io_wait(&mut self, io_wait_future_ptr: *const IOWaitFuture) -> Result<(), io::Error>;

    fn as_any(&mut self) -> &mut dyn any::Any
    where
        Self: 'static;
}

pub struct IOManagerHolder {
    pub(crate) io_manager: Box<dyn IOManager>,
}

pub trait IOManagerCreateOptions {
    fn try_new_io_manager(&self) -> io::Result<IOManagerHolder>;
}

/// No operation IOManager.
///
pub(crate) struct NoopIOManager {}

impl NoopIOManager {
    pub(crate) fn new() -> Self {
        NoopIOManager {}
    }
}

impl IOManager for NoopIOManager {
    fn has_active_io(&self) -> bool {
        false
    }

    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)> {
        None
    }

    fn register_io_wait(&mut self, _: *const IOWaitFuture) -> Result<(), io::Error> {
        Ok(())
    }

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
pub struct NoopIOManagerCreateOptions {}

impl NoopIOManagerCreateOptions {
    pub fn new() -> Self {
        NoopIOManagerCreateOptions {}
    }
}

impl Default for NoopIOManagerCreateOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl IOManagerCreateOptions for NoopIOManagerCreateOptions {
    fn try_new_io_manager(&self) -> io::Result<IOManagerHolder> {
        Ok(IOManagerHolder {
            io_manager: Box::new(NoopIOManager::new()),
        })
    }
}
