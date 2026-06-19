//! Panic-safe initialization barrier for multi-thread startup.
//!
//! Provides `InitBarrier`, which combines a `CountdownEvent` and `Barrier`
//! so that if any worker thread panics during initialization, remaining
//! threads are unblocked rather than hanging forever.

use std::sync::{Arc, Barrier, BarrierWaitResult};

use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;

/// Panic-safe synchronization barrier for multi-thread initialization.
///
/// Wraps a `CountdownEvent` and `Barrier`. Each worker thread calls
/// `guard().ready()` to signal completion and synchronize. If a thread
/// panics, the guard's `Drop` completes the remaining steps so no
/// thread hangs.
pub(crate) struct InitBarrier {
    countdown: Arc<CountdownEvent>,
    barrier: Arc<Barrier>,
}

impl InitBarrier {
    /// Creates a barrier for `thread_count` worker threads plus one main thread.
    pub(crate) fn new(thread_count: usize) -> Self {
        Self {
            countdown: Arc::new(CountdownEvent::new(thread_count)),
            barrier: Arc::new(Barrier::new(thread_count + 1)),
        }
    }

    /// Creates a guard for a worker thread.
    pub(crate) fn guard(&self) -> InitBarrierGuard {
        InitBarrierGuard {
            countdown: Arc::clone(&self.countdown),
            barrier: Arc::clone(&self.barrier),
            signaled: false,
            synchronized: false,
        }
    }

    /// Main thread: wait for all workers to signal, then synchronize.
    pub(crate) fn wait(&self) -> BarrierWaitResult {
        self.countdown.wait();
        self.barrier.wait()
    }
}

pub(crate) struct InitBarrierGuard {
    countdown: Arc<CountdownEvent>,
    barrier: Arc<Barrier>,
    signaled: bool,
    synchronized: bool,
}

impl InitBarrierGuard {
    /// Signal that this thread's initialization is complete, then wait
    /// for all threads and the main thread to synchronize.
    pub(crate) fn ready(&mut self) {
        self.countdown.signal();
        self.signaled = true;
        self.countdown.wait();
        self.barrier.wait();
        self.synchronized = true;
    }
}

impl Drop for InitBarrierGuard {
    fn drop(&mut self) {
        if !self.signaled {
            self.countdown.signal();
        }
        if !self.synchronized {
            self.countdown.wait();
            self.barrier.wait();
        }
    }
}
