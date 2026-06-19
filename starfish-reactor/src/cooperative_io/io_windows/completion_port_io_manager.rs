//! IOCP-based I/O manager for the reactor.
//!
//! Implements `IOManager` on a Windows I/O completion port owned by its
//! reactor: handles are associated via `prepare_io`, and the reactor thread
//! itself dequeues completion packets with `GetQueuedCompletionStatusEx` —
//! non-blocking from `completed_io`, blocking (deadline-bounded) from
//! `wait_for_io`. There is no dedicated completion-handler thread; the former
//! handler thread, its `SegQueue`, and the CAS-on-`hEvent` rendezvous were
//! removed when idle sleeping landed (former trackers #5/#59) — the reactor blocking
//! directly in the kernel dequeue is both the sleep mechanism and a net
//! simplification.
//!
//! # Invariants
//!
//! 1. **`OVERLAPPED.hEvent` routes packets to futures, single-threaded.**
//!    `hEvent` must be 0 while the overlapped I/O call is issued — Windows
//!    captures the field at submission, and a set low bit at that moment
//!    suppresses the completion-port notification entirely. The kernel never
//!    touches the field afterwards, so `register_io_wait` stores the owning
//!    `IOWaitFuture`'s address in it and the dequeue path reads it back.
//!    Both run on the reactor thread, and the reactor never dequeues between
//!    issuing an operation and registering it (the poll that parks the task
//!    runs in between, with no `completed_io`/`wait_for_io` call), so every
//!    dequeued packet's `hEvent` already carries its future. No atomics —
//!    the cross-thread CAS rendezvous this replaced died with the handler
//!    thread.
//! 2. **`active_io` counts undelivered operations.** Incremented at
//!    registration, decremented when a completion is *delivered* to the
//!    reactor — packets buffered in `completed` still count, so
//!    `has_active_io()` keeps the reactor loop alive while a completion
//!    awaits delivery. Wake packets are never counted: a pending wake must
//!    not keep the reactor alive (or it could never exit).
//! 3. **Sleeping is interruptible** (mirrors the io_uring backend's
//!    invariant 4). `wait_for_io` blocks in `GetQueuedCompletionStatusEx`;
//!    `wake()` posts a `WAKEUP_KEY` packet via `PostQueuedCompletionStatus`,
//!    which is documented thread-safe and needs no reactor state or locks.
//!    GQCSEx dequeues atomically — packets posted just before the call are
//!    returned, never lost — and nothing between the reactor's sleep
//!    decision and the block consumes packets, so a wake cannot be destroyed
//!    pre-block (the io_uring backend needs a `wake_observed` re-check for
//!    exactly that window; IOCP has no analog). Wake packets do not coalesce
//!    (one per `wake()`); stale ones are discarded at the next dequeue,
//!    which is safe because any work enqueued before a wake is observed by
//!    the reactor's post-sleeping-flag queue re-check before it blocks
//!    again.

use std::collections::VecDeque;
use std::time::Duration;
use std::{io, ptr};

use ctor::{ctor, dtor};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::WinSock::{WSACleanup, WSADATA, WSAStartup};
use windows::Win32::System::IO::CancelIoEx;

use crate::cooperative_io::io_manager::{
    IOManager, IOManagerCreateOptions, IOManagerHolder, WaitResult,
};
use crate::reactor::FutureRuntime;

use super::io_wait_future::IOWaitFuture;
use super::iocp::{CompletionStatus, IoCompletionPort};

/// Completion key reserved for wake packets posted by [`IOManager::wake`].
/// Real handles are associated with the manager's address as their key
/// (`prepare_io`), which is never 0.
const WAKEUP_KEY: usize = 0;

/// `GetQueuedCompletionStatusEx` "wait forever" timeout (winbase `INFINITE`).
const INFINITE: u32 = u32::MAX;

/// Max completion packets dequeued from the port per syscall. Leftovers stay
/// queued in the port and surface on the next dequeue.
const DEQUEUE_BATCH: usize = 16;

pub(crate) struct CompletionPortIOManager {
    /// The completion port, owned per manager (per reactor thread). The
    /// reactor thread is the only dequeuer; `wake()` may post from any
    /// thread (`PostQueuedCompletionStatus` is documented thread-safe).
    io_completion_port: IoCompletionPort,
    /// Completions dequeued from the port but not yet delivered to the
    /// reactor: `completed_io` delivers one per call and decrements
    /// `active_io` at delivery (module invariant 2).
    completed: VecDeque<CompletionStatus>,
    active_io: usize,
}

impl CompletionPortIOManager {
    pub fn try_new() -> io::Result<Self> {
        Ok(CompletionPortIOManager {
            // One concurrent dequeuer: the owning reactor thread.
            io_completion_port: IoCompletionPort::new(1)?,
            completed: VecDeque::new(),
            active_io: 0,
        })
    }

    pub fn prepare_io(&mut self, handle: HANDLE) -> Result<(), io::Error> {
        let io_manager_ptr: *const CompletionPortIOManager = ptr::addr_of!(*self);

        // Registers handle with this manager's IO completion port. The key is
        // only compared against WAKEUP_KEY when packets are dequeued (packet
        // routing goes through hEvent — module invariant 1); the manager
        // address is used because it is never 0 and aids debugging.
        //
        self.io_completion_port
            .associate(handle, io_manager_ptr as usize)
    }

    /// Classifies one batch of dequeued packets: buffers real completions
    /// into `completed`, discards wake packets (module invariant 3). Returns
    /// true if at least one wake packet was present.
    fn buffer_dequeued(&mut self, dequeued: &[CompletionStatus]) -> bool {
        let mut woken = false;

        for completion_status in dequeued {
            if completion_status.completion_key == WAKEUP_KEY {
                debug_assert!(
                    completion_status.overlapped.is_null(),
                    "wake packets carry no OVERLAPPED"
                );
                woken = true;
            } else {
                debug_assert!(
                    !completion_status.overlapped.is_null(),
                    "real completion packets carry an OVERLAPPED"
                );
                self.completed.push_back(*completion_status);
            }
        }

        woken
    }

    /// Non-blocking dequeue of one batch from the port into `completed`.
    fn poll_completions(&mut self) {
        let mut dequeued = [CompletionStatus::new(); DEQUEUE_BATCH];

        // Timeout 0 = poll; an empty port reports WAIT_TIMEOUT as Err, which
        // simply means nothing was ready.
        if let Ok(count) = self.io_completion_port.get_many_queued(&mut dequeued, 0) {
            self.buffer_dequeued(&dequeued[..count]);
        }
    }
}

impl IOManager for CompletionPortIOManager {
    fn has_active_io(&self) -> bool {
        self.active_io != 0
    }

    /// See [`IOManager::completed_io`].
    ///
    /// Every delivered packet is guaranteed to have `future_runtime` set,
    /// because its `hEvent` was written by `register_io_wait` (which runs
    /// strictly before the packet can be dequeued — module invariant 1) and
    /// registration follows `set_future_runtime` in the reactor's IOWait
    /// scheduling path.
    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)> {
        if self.completed.is_empty() {
            if self.active_io == 0 {
                // Nothing in flight: skip the syscall. Wake packets may sit
                // in the port; they are consumed (and discarded) by the next
                // dequeue and must not be confused with completions.
                return None;
            }

            self.poll_completions();
        }

        let io_completed_status = self.completed.pop_front()?;

        // hEvent carries the IOWaitFuture address stored by register_io_wait
        // (module invariant 1).
        //
        // SAFETY: the packet's OVERLAPPED lives in the parked task's async-fn
        // frame, which stays alive until the task's runtime is delivered
        // below; this (reactor) thread is the only accessor post-submission.
        let io_wait_future_ptr =
            unsafe { (*io_completed_status.overlapped).hEvent.0 } as *mut IOWaitFuture;

        debug_assert!(
            !io_wait_future_ptr.is_null(),
            "completion packet for an unregistered operation — handle used \
             from a foreign reactor thread? (tracker #47)"
        );

        // SAFETY: module invariant 1 — register_io_wait stored this pointer
        // before the packet could be dequeued, and the future stays parked
        // (alive) until its runtime is handed back right here.
        let io_wait_future = unsafe { &mut *io_wait_future_ptr };

        assert!(io_wait_future.is_completed());
        self.active_io -= 1;

        Some((
            io_wait_future.take_future_runtime().unwrap(),
            io_wait_future_ptr,
        ))
    }

    /// See [`IOManager::register_io_wait`].
    ///
    /// Stores the future's address in `OVERLAPPED.hEvent` so the dequeue
    /// path can route the completion packet back to it (module invariant 1).
    fn register_io_wait(
        &mut self,
        io_wait_future_ptr: *const IOWaitFuture,
    ) -> Result<(), io::Error> {
        // SAFETY: the reactor passes a pointer to a live, pinned IOWaitFuture
        // it has just parked (ScheduleReason::IOWait handling).
        let io_wait_future = unsafe { &*io_wait_future_ptr };

        // SAFETY: `overlapped` is valid for the future's lifetime (both live
        // in the same async-fn frame). No concurrent access: the kernel
        // captured hEvent (0) at submission and never touches the field
        // again, and this (reactor) thread is the only other accessor.
        unsafe { (*io_wait_future.overlapped).hEvent = HANDLE(io_wait_future_ptr as isize) };

        self.active_io += 1;

        Ok(())
    }

    /// See [`IOManager::cancel_io_wait`].
    ///
    /// The cancelled operation still completes through the port (with
    /// `ERROR_OPERATION_ABORTED`), so its accounting slot is released by the
    /// normal delivery path.
    fn cancel_io_wait(&mut self, io_wait_future_ptr: *const IOWaitFuture) -> Result<(), io::Error> {
        let io_future_wait = unsafe { &*io_wait_future_ptr };

        unsafe { CancelIoEx(io_future_wait.handle, Some(io_future_wait.overlapped)) }?;

        Ok(())
    }

    /// See [`IOManager::wait_for_io`].
    ///
    /// Race-free against both completion arrival and `wake()`:
    /// `GetQueuedCompletionStatusEx` dequeues atomically, so packets posted
    /// at any point before the call are returned rather than lost (module
    /// invariant 3).
    fn wait_for_io(&mut self, timeout: Option<Duration>) -> WaitResult {
        if !self.completed.is_empty() {
            return WaitResult::IoCompleted;
        }

        let timeout_ms = match timeout {
            None => INFINITE,
            // Finite waits clamp to 1..=INFINITE-1 ms: 0 would poll instead
            // of block (sub-millisecond deadlines busy-loop), and u32::MAX
            // is the INFINITE sentinel — an accidental unbounded sleep.
            // Millisecond truncation is compensated by the 1ms floor and by
            // the reactor re-deriving the remaining deadline after every
            // wait; GQCSEx timeouts are coarse anyway (~15.6ms scheduler
            // granularity), which the reactor's spin window and the test
            // margins absorb — do not try to spin it away here.
            Some(duration) => duration.as_millis().clamp(1, (INFINITE - 1) as u128) as u32,
        };

        let mut dequeued = [CompletionStatus::new(); DEQUEUE_BATCH];

        match self
            .io_completion_port
            .get_many_queued(&mut dequeued, timeout_ms)
        {
            Ok(count) => {
                let woken = self.buffer_dequeued(&dequeued[..count]);

                if !self.completed.is_empty() {
                    WaitResult::IoCompleted
                } else if woken {
                    WaitResult::Woken
                } else {
                    WaitResult::TimedOut
                }
            }
            // WAIT_TIMEOUT, or any other failure: report TimedOut — the
            // result is advisory and the reactor re-scans its queues.
            Err(_) => WaitResult::TimedOut,
        }
    }

    /// See [`IOManager::wake`]. Thread-safe: `PostQueuedCompletionStatus` is
    /// documented safe from any thread, and no manager state is touched.
    fn wake(&self) {
        let _ = self.io_completion_port.post_queued(CompletionStatus {
            byte_count: 0,
            completion_key: WAKEUP_KEY,
            overlapped: ptr::null_mut(),
        });
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any
    where
        Self: 'static,
    {
        self
    }
}

#[ctor]
fn initialize_wsa() {
    unsafe {
        let mut wsa_data = WSADATA::default();
        let result = WSAStartup(0x0202, &mut wsa_data);

        if result != 0 {
            panic!(
                "WSA initialization failed: {}",
                io::Error::from_raw_os_error(result)
            );
        }
    }
}

#[dtor]
fn cleanup_wsa() {
    let _ = unsafe { WSACleanup() };
}

// Implements IOManager create options.
//
pub struct CompletionPortIOManagerCreateOptions {}

impl Default for CompletionPortIOManagerCreateOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl CompletionPortIOManagerCreateOptions {
    pub fn new() -> Self {
        CompletionPortIOManagerCreateOptions {}
    }
}

impl IOManagerCreateOptions for CompletionPortIOManagerCreateOptions {
    /// Each manager (reactor) gets its own completion port: the owning
    /// reactor thread is the only dequeuer (module invariant 1 relies on
    /// this), unlike the former shared-port + handler-thread design.
    fn try_new_io_manager(&self) -> io::Result<IOManagerHolder> {
        Ok(IOManagerHolder {
            io_manager: Box::new(CompletionPortIOManager::try_new()?),
        })
    }
}
