//! I/O manager trait and associated types.
//!
//! Defines `IOManager`, the platform-agnostic trait for submitting and completing
//! async I/O operations, along with `IOManagerHolder` and `IOManagerCreateOptions`
//! for reactor-level I/O manager lifecycle management. `local_io_manager<T>()`
//! provides a safe accessor to the thread-local `IOManager` downcast to a concrete
//! type, returning `None` when no reactor is running on the current thread.

use std::any;
use std::io;

use crate::reactor::FutureRuntime;

use super::io_wait_future::IOWaitFuture;

/// Returns a reference to the thread-local `IOManager` downcast to `T`, if a
/// reactor is running on the current thread.
pub(crate) fn local_io_manager<T: IOManager + 'static>() -> Option<&'static mut T> {
    use crate::reactor::Reactor;

    if !Reactor::has_local_instance() {
        return None;
    }

    Reactor::local_instance().io_manager::<T>()
}

/// Outcome of a blocking [`IOManager::wait_for_io`] call.
///
/// Advisory only: the reactor re-scans all of its queues after any wait
/// outcome, so a conservative result (e.g. `TimedOut` instead of `Woken`)
/// affects latency accounting, never correctness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitResult {
    /// At least one I/O completion is ready for delivery via `completed_io`.
    IoCompleted,
    /// The wait was interrupted by [`IOManager::wake`].
    Woken,
    /// The timeout expired (or the backend cannot sleep and returned at once).
    TimedOut,
}

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

    /// Blocks the calling (reactor) thread until an I/O completion is ready,
    /// [`IOManager::wake`] is called from another thread, or `timeout`
    /// expires (`None` = wait indefinitely).
    ///
    /// The implementation must be race-free against its own completion
    /// source: entering the wait must atomically observe completions that
    /// arrived just before (io_uring_enter / kevent / GQCSEx all guarantee
    /// this). The reactor separately guards the external-futures queue with
    /// its own sleeping-flag handshake before calling this.
    ///
    /// The default keeps the legacy busy-spin behavior — it returns
    /// `TimedOut` immediately and never blocks — so backends that do not
    /// sleep remain correct, just inefficient: `NoopIOManager` (used by the
    /// scheduling-only benchmarks) and the Windows `PoolingIOManager`
    /// fallback both rely on it.
    fn wait_for_io(&mut self, _timeout: Option<std::time::Duration>) -> WaitResult {
        WaitResult::TimedOut
    }

    /// Wakes a thread blocked in [`IOManager::wait_for_io`]. Must be safe to
    /// call from any thread (`&self`, no reactor state). Pairs with the
    /// `wait_for_io` default: a backend that never sleeps needs no wake.
    fn wake(&self) {}

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
