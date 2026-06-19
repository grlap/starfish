//! io_uring-based I/O manager for the reactor.
//!
//! Implements `IOManager` using Linux's io_uring interface to submit and reap
//! async file and network I/O operations, driving cooperative future completion.
//!
//! # Lifecycle invariants
//!
//! The kernel reads submission entries (and reads/writes the buffers they point
//! to) asynchronously, so the central memory-safety rule is:
//!
//! 1. **A pushed SQE is a commitment.** Once an entry has been pushed to the
//!    submission ring, `register_io_wait` must NOT report failure: the reactor's
//!    error path completes the task immediately, freeing the `IOWaitFuture` and
//!    the I/O buffers while the ring still references them (use-after-free, the
//!    former bug #54). Transient `submit()` errors (`EBUSY` CQ backpressure under
//!    `IORING_FEAT_NODROP`, `EAGAIN`, `EINTR`) leave the entry staged in the ring
//!    and are retried from `completed_io()`. Only a push that never entered the
//!    ring may fail the operation.
//! 2. **`active_io` counts undelivered operations.** It is incremented when an
//!    entry is staged and decremented when its completion is *delivered* to the
//!    reactor (not when reaped from the kernel ring), so `has_active_io()` keeps
//!    the reactor loop alive while completions sit in the internal buffer
//!    (`completed_io` drains the whole completion ring per call to release CQ
//!    backpressure, then delivers one completion at a time). Cancellation CQEs
//!    (`user_data == 0`) are never delivered and decrement at reap.
//! 3. **Drop drains the kernel.** Tasks parked on I/O are self-referential (the
//!    task box lives inside its own `IOWaitFuture`), so nothing else frees them.
//!    `Drop` flushes staged entries, cancels all in-flight operations with one
//!    `IORING_ASYNC_CANCEL_ANY` request (kernel 5.19+), reaps until none
//!    remain, and only then drops the task boxes — guaranteeing the kernel is
//!    finished with the buffers before they are freed (the former teardown
//!    leak, bug #58). On pre-5.19 kernels the cancel request fails and
//!    unresolved operations are deliberately leaked instead.
//! 4. **Sleeping is interruptible.** `wait_for_io` blocks in
//!    `io_uring_enter(GETEVENTS)` (bounded by an `EXT_ARG` timespec when a
//!    timeout is given). A multishot `PollAdd` on an internal eventfd is kept
//!    armed with `user_data == WAKE_TOKEN`; `wake()` (any thread) writes the
//!    eventfd, producing a CQE that ends the wait. Entering the wait observes
//!    already-arrived CQEs atomically, and a wake CQE consumed by the
//!    prologue's own reaps is recorded in `wake_observed` and re-checked
//!    immediately before blocking — so no completion or wake can be lost.
//!    The wake poll is internal — never counted in `active_io`/`kernel_owed`,
//!    so it cannot keep the reactor alive or stall the Drop drain (its
//!    ECANCELED CQE at teardown is consumed without re-arming).

use io_uring::IoUring;
use io_uring::cqueue;
use io_uring::opcode;
use io_uring::types;
use libc::{EAGAIN, EBUSY, ECANCELED, EINTR};
use std::any;
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use crate::cooperative_io::io_manager::{
    IOManager, IOManagerCreateOptions, IOManagerHolder, WaitResult,
};
use crate::reactor::FutureRuntime;

use super::io_wait_future::IOWaitFuture;

/// `user_data` of the eventfd wake-poll CQEs (invariant 4). Distinct from
/// real operations (heap addresses) and cancellations (0); never counted in
/// `active_io`.
const WAKE_TOKEN: u64 = u64::MAX;

pub(crate) struct UringIOManager {
    io_uring: IoUring,

    /// Operations staged/submitted but not yet delivered to the reactor
    /// (invariant 2 in the module docs).
    active_io: usize,

    /// True when entries sit in the submission ring because `submit()` failed
    /// transiently; retried from `completed_io()` (invariant 1).
    needs_submit: bool,

    /// Completions reaped from the kernel ring but not yet delivered.
    /// `active_io - completed.len()` is therefore the number of CQEs the
    /// kernel still owes us — the `Drop` drain's termination predicate
    /// (invariant 3).
    completed: VecDeque<(FutureRuntime, *const IOWaitFuture)>,

    /// Cross-thread wake channel for `wait_for_io` (invariant 4): a multishot
    /// `PollAdd` on this eventfd is armed in the ring; `wake()` writes 8
    /// bytes, completing a `WAKE_TOKEN` CQE that interrupts a blocked
    /// `io_uring_enter`. Declared after `io_uring` so the ring (which
    /// references the fd) is dropped first.
    wake_fd: OwnedFd,

    /// Whether the multishot wake poll is currently armed in the ring. When
    /// false (pre-5.13 kernel rejected multishot, or arming failed),
    /// `wait_for_io` refuses unbounded sleeps and degrades to short bounded
    /// waits so wakes are never required for liveness.
    wake_poll_armed: bool,

    /// Set by `reap_completions` whenever a WAKE_TOKEN CQE is consumed.
    /// `wait_for_io` checks-and-clears it immediately before blocking: a wake
    /// reaped during its prologue must end the wait, not be silently
    /// destroyed (the eventfd is already drained and the CQE consumed, so no
    /// other trace of the wake survives — the lost-wakeup found in review).
    wake_observed: bool,

    /// Latched when a WAKE_TOKEN CQE reports an error other than ECANCELED
    /// (e.g. EINVAL: multishot poll unsupported, pre-5.13). Prevents the
    /// arm -> EINVAL -> re-arm hot loop; the bounded-wait degraded mode in
    /// `wait_for_io` then engages permanently.
    wake_poll_unsupported: bool,
}

/// [`UringIOManager`].
/// See [`IOManager`].
///
const DEFAULT_QUEUE_SIZE: u32 = 1024;

/// Returns true for `submit()` errors that resolve on retry once completions
/// are reaped (CQ backpressure) or the call is repeated.
fn is_transient_submit_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(code) if code == EBUSY || code == EAGAIN || code == EINTR)
}

impl UringIOManager {
    pub(crate) fn try_new(queue_size: u32) -> io::Result<Self> {
        let io_uring = IoUring::new(queue_size)?;

        // SAFETY: eventfd returns a fresh fd we exclusively own (or -1).
        let raw_wake_fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if raw_wake_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just created above, not owned by anything else.
        let wake_fd = unsafe { OwnedFd::from_raw_fd(raw_wake_fd) };

        let mut manager = UringIOManager {
            io_uring,
            active_io: 0,
            needs_submit: false,
            completed: VecDeque::new(),
            wake_fd,
            wake_poll_armed: false,
            wake_observed: false,
            wake_poll_unsupported: false,
        };

        manager.arm_wake_poll();

        Ok(manager)
    }

    /// Arms (or re-arms) the multishot eventfd poll that lets `wake()`
    /// interrupt a blocked `wait_for_io`. Multishot needs kernel 5.13+; on
    /// failure `wake_poll_armed` stays false and sleeps degrade to bounded.
    fn arm_wake_poll(&mut self) {
        if self.wake_poll_unsupported {
            return;
        }

        let poll_entry =
            opcode::PollAdd::new(types::Fd(self.wake_fd.as_raw_fd()), libc::POLLIN as u32)
                .multi(true)
                .build()
                .user_data(WAKE_TOKEN);

        if self.push_entry(&poll_entry) {
            self.flush_submissions();
            self.wake_poll_armed = true;
        }
    }

    /// Resets the eventfd counter after a wake-poll CQE. A single read
    /// returns-and-zeroes the whole counter (non-semaphore eventfd), so
    /// coalesced wakes cost one syscall.
    fn drain_wake_fd(&self) {
        let mut counter = [0u8; 8];
        // SAFETY: valid fd; 8-byte buffer is what eventfd reads require.
        unsafe {
            libc::read(
                self.wake_fd.as_raw_fd(),
                counter.as_mut_ptr() as *mut libc::c_void,
                8,
            )
        };
    }

    /// Number of CQEs the kernel still owes us: undelivered operations
    /// (`active_io`) minus those already reaped into the buffer. Every staged
    /// operation and cancellation produces exactly one CQE, so this reaches
    /// zero exactly when the kernel is done with all buffers we know about.
    fn kernel_owed(&self) -> usize {
        self.active_io - self.completed.len()
    }

    /// Hands staged submission-ring entries to the kernel. Failures (transient
    /// or not) keep the entries staged for a later retry. Per invariant 1, a
    /// submit error must never surface as an operation failure — that at worst
    /// delays the task, whereas failing it frees memory the kernel still owns.
    fn flush_submissions(&mut self) {
        let _ = self.io_uring.submit();
        self.needs_submit = !self.io_uring.submission().is_empty();
    }

    /// Pushes one entry, flushing (and reaping, to relieve CQ backpressure)
    /// once if the submission ring is full. Returns false if the entry could
    /// not be staged — in that case the ring holds no reference to it.
    fn push_entry(&mut self, queue_entry: &io_uring::squeue::Entry) -> bool {
        // SAFETY: the entry's buffers and fd are owned by the IOWaitFuture's
        // task, which stays alive until the operation's CQE is delivered or the
        // Drop drain reaps it (module invariants 1 and 3).
        if unsafe { self.io_uring.submission().push(queue_entry) }.is_ok() {
            return true;
        }

        // Submission ring full (possible when prior submits hit EBUSY and left
        // entries staged). Reap completions to relieve CQ backpressure, flush,
        // then retry once.
        self.reap_completions();
        self.flush_submissions();

        // SAFETY: same ownership argument as above.
        unsafe { self.io_uring.submission().push(queue_entry) }.is_ok()
    }

    /// Drains every available CQE from the kernel ring into `completed`,
    /// freeing completion-ring space (under `IORING_FEAT_NODROP` a full CQ
    /// makes `submit()` return EBUSY rather than dropping events — the former
    /// bug #60 accounting hole).
    ///
    /// When the completion ring overflowed, the kernel parks the excess CQEs
    /// in a backlog that is only moved into the ring by an `io_uring_enter`
    /// with GETEVENTS — draining the ring alone strands them (and `active_io`
    /// would never reach zero). The overflow flag gates a `submit_and_wait(1)`
    /// that cannot block: the non-empty backlog itself satisfies the wait.
    fn reap_completions(&mut self) {
        loop {
            let Some(queue_entry) = self.io_uring.completion().next() else {
                if self.io_uring.submission().cq_overflow() {
                    if self.io_uring.submitter().submit_and_wait(1).is_err() {
                        break;
                    }
                    continue;
                }
                break;
            };

            if queue_entry.user_data() == WAKE_TOKEN {
                // Wake-poll CQE (invariant 4): not an operation, never counted
                // in `active_io`. On success drain the eventfd counter and
                // re-arm if the multishot chain ended; on error (ECANCELED at
                // teardown, EINVAL pre-5.13) leave it disarmed — wait_for_io
                // then degrades to bounded sleeps.
                if queue_entry.result() >= 0 {
                    self.wake_observed = true;
                    self.drain_wake_fd();
                    if !cqueue::more(queue_entry.flags()) {
                        self.wake_poll_armed = false;
                        self.arm_wake_poll();
                    }
                } else {
                    self.wake_poll_armed = false;
                    if queue_entry.result() != -ECANCELED {
                        // EINVAL (multishot unsupported) or another permanent
                        // rejection: latch, or every sleep attempt re-arms and
                        // hot-loops on fresh error CQEs.
                        self.wake_poll_unsupported = true;
                    }
                }
                continue;
            }

            let io_wait_future_ptr = queue_entry.user_data() as *mut IOWaitFuture;

            if io_wait_future_ptr.is_null() {
                // AsyncCancel CQE (no associated future): never delivered, so
                // its slot in `active_io` is released at reap time.
                self.active_io -= 1;
                continue;
            }

            // SAFETY: non-zero user_data is the address of an IOWaitFuture
            // whose owning task box has not been delivered or dropped yet
            // (module invariants 1 and 3).
            let completed_io_wait_future = unsafe { &mut *io_wait_future_ptr };

            let result = queue_entry.result();
            if result >= 0 {
                completed_io_wait_future.set_io_result(Ok(result as usize));
            } else if result == -ECANCELED {
                // The reactor cancels in-flight IO via AsyncCancel when a
                // timeout expires.  Report it as TimedOut so callers that
                // match on ErrorKind::TimedOut handle it correctly.
                completed_io_wait_future.set_io_result(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "operation timed out",
                )));
            } else {
                completed_io_wait_future.set_io_result(Err(io::Error::from_raw_os_error(-result)));
            }

            let future_runtime = completed_io_wait_future.take_future_runtime().unwrap();

            self.completed
                .push_back((future_runtime, io_wait_future_ptr as *const IOWaitFuture));
        }
    }
}

impl IOManager for UringIOManager {
    fn has_active_io(&self) -> bool {
        self.active_io != 0
    }

    /// See [`IOManager::completed_io`].
    ///
    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)> {
        if self.active_io == 0 {
            return None;
        }

        // Retry entries stranded by a transient submit error before looking
        // for completions: their CQEs cannot arrive until the kernel sees them.
        if self.needs_submit {
            self.flush_submissions();
        }

        self.reap_completions();

        let delivery = self.completed.pop_front();

        if delivery.is_some() {
            // Delivered to the reactor: this operation no longer holds the
            // loop alive (invariant 2).
            self.active_io -= 1;
        }

        delivery
    }

    /// See [`IOManager::register_io_wait`].
    ///
    /// Stages a single SQE. Returns `Err` ONLY when the entry never entered
    /// the submission ring (the reactor's error path then safely completes the
    /// task). After a successful push, transient submit errors are absorbed
    /// and retried — see module invariant 1.
    fn register_io_wait(
        &mut self,
        io_wait_future_ptr: *const super::io_wait_future::IOWaitFuture,
    ) -> Result<(), io::Error> {
        // SAFETY: the reactor passes a pointer to a live, pinned IOWaitFuture
        // that it has just parked (ScheduleReason::IOWait handling).
        let io_wait_future = unsafe { &mut *{ io_wait_future_ptr as *mut IOWaitFuture } };

        let mut queue_entry = io_wait_future.queue_entry.take().unwrap();
        queue_entry = queue_entry.user_data(io_wait_future_ptr as u64);

        if !self.push_entry(&queue_entry) {
            // Nothing was staged; failing the operation is safe.
            return Err(io::Error::from_raw_os_error(EBUSY));
        }

        // Point of commitment: the ring references the entry from here on.
        self.active_io += 1;

        // Reap before submitting: every submission is future completion-ring
        // pressure (one CQE owed), so clearing the ring here — where pressure
        // is created — keeps `submit()` from ever hitting EBUSY backpressure
        // in steady state. Skipped when this is the only outstanding
        // operation (just counted above): the ring can then only hold
        // uncounted WAKE_TOKEN CQEs, which are harmless to leave (bounded by
        // eventfd coalescing, consumed by the next reap).
        if self.active_io > 1 {
            self.reap_completions();
        }

        match self.io_uring.submit() {
            Ok(_) => {
                self.needs_submit = !self.io_uring.submission().is_empty();
            }
            Err(ref error) if is_transient_submit_error(error) => {
                self.needs_submit = true;
            }
            Err(_) => {
                // Even a non-transient error cannot fail the operation now
                // (invariant 1); leave the entry staged and keep retrying.
                self.needs_submit = true;
            }
        }

        Ok(())
    }

    /// See [`IOManager::cancel_io_wait`].
    ///
    /// Submits an `AsyncCancel` SQE that cancels the in-flight IO
    /// identified by the future pointer (used as user_data). The cancel
    /// CQE uses user_data=0 so `reap_completions` ignores it.
    ///
    fn cancel_io_wait(&mut self, io_wait_future_ptr: *const IOWaitFuture) -> Result<(), io::Error> {
        let cancel_op = opcode::AsyncCancel::new(io_wait_future_ptr as u64)
            .build()
            .user_data(0);

        if !self.push_entry(&cancel_op) {
            // The cancel never entered the ring. The target operation simply
            // keeps running and completes normally later; reporting the error
            // is safe.
            return Err(io::Error::from_raw_os_error(EBUSY));
        }

        self.active_io += 1;
        self.reap_completions();
        self.flush_submissions();
        Ok(())
    }

    /// See [`IOManager::wait_for_io`].
    ///
    /// Race-free against completions: `io_uring_enter(GETEVENTS)` observes
    /// CQEs that arrived before the wait as part of entering it, and the
    /// armed wake poll turns `wake()` into a CQE — so neither I/O nor a wake
    /// can be lost between the final reap below and the kernel block.
    fn wait_for_io(&mut self, timeout: Option<Duration>) -> WaitResult {
        // Wakes consumed before this call pre-date the reactor's sleeping
        // flag and queue re-check; only wakes reaped during the prologue
        // below must abort the sleep.
        self.wake_observed = false;

        if !self.completed.is_empty() {
            return WaitResult::IoCompleted;
        }

        if self.needs_submit {
            self.flush_submissions();
        }

        self.reap_completions();
        if !self.completed.is_empty() {
            return WaitResult::IoCompleted;
        }

        if !self.wake_poll_armed {
            self.arm_wake_poll();
        }

        // Pre-block re-check: the reap above (or a reap nested inside
        // arm_wake_poll's push) may have CONSUMED a wake CQE — the eventfd is
        // already drained, so blocking now would destroy the wake (review
        // finding, reproduced standalone: the enter slept its full timeout
        // after a delivered wake). Same for completions buffered by a nested
        // reap after the fast-path check. Spurious early returns are fine:
        // WaitResult is advisory and the reactor re-scans its queues.
        if std::mem::take(&mut self.wake_observed) {
            return WaitResult::Woken;
        }
        if !self.completed.is_empty() {
            return WaitResult::IoCompleted;
        }

        // Without an armed wake poll, an unbounded sleep could never be
        // interrupted by wake(); cap it so liveness never depends on the
        // wake channel.
        let effective_timeout = if self.wake_poll_armed {
            timeout
        } else {
            Some(
                timeout
                    .unwrap_or(Duration::from_millis(1))
                    .min(Duration::from_millis(1)),
            )
        };

        let wait_outcome = match effective_timeout {
            Some(duration) => {
                let timespec = types::Timespec::new()
                    .sec(duration.as_secs())
                    .nsec(duration.subsec_nanos());
                let submit_args = types::SubmitArgs::new().timespec(&timespec);
                self.io_uring.submitter().submit_with_args(1, &submit_args)
            }
            None => self.io_uring.submit_and_wait(1),
        };

        let timed_out = matches!(
            &wait_outcome,
            Err(error) if error.raw_os_error() == Some(libc::ETIME)
        );

        self.reap_completions();

        if !self.completed.is_empty() {
            WaitResult::IoCompleted
        } else if timed_out {
            WaitResult::TimedOut
        } else {
            // Wake CQE consumed by the reap, or a transient enter error —
            // either way the reactor re-scans its queues.
            WaitResult::Woken
        }
    }

    /// See [`IOManager::wake`]. Thread-safe: a plain 8-byte write to the
    /// eventfd; coalesces (counter), drained in one read by the sleeper.
    fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: valid eventfd for the manager's lifetime; 8-byte write of
        // a u64 is the eventfd contract; safe from any thread.
        unsafe {
            libc::write(
                self.wake_fd.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            )
        };
    }

    fn as_any(&mut self) -> &mut dyn any::Any
    where
        Self: 'static,
    {
        self
    }
}

impl Drop for UringIOManager {
    /// Drains the kernel before the ring (and the buffers owned by parked
    /// tasks) go away: flush staged entries, cancel everything in flight, reap
    /// until nothing remains, then free the task boxes (module invariant 3).
    fn drop(&mut self) {
        // Flush any entries stranded by transient submit errors.
        let mut flush_attempts = 0;
        while self.needs_submit && flush_attempts < 64 {
            self.reap_completions();
            self.flush_submissions();
            flush_attempts += 1;
        }

        // Cancel every operation the kernel still owns with a single
        // IORING_ASYNC_CANCEL_ANY request (kernel 5.19+; on older kernels the
        // request fails and unresolved operations fall through to the bounded
        // leak below). Every kernel-owed operation produces exactly one CQE,
        // so `active_io - completed.len()` (undelivered minus already-reaped)
        // is the number of CQEs still owed — the drain's termination predicate.
        if self.kernel_owed() > 0 {
            let cancel_all = opcode::AsyncCancel2::new(types::CancelBuilder::any())
                .build()
                .user_data(0);
            if self.push_entry(&cancel_all) {
                self.active_io += 1;
                self.flush_submissions();
            }
        }

        // Reap until every kernel-owed CQE has arrived. Bounded in TIME, not
        // just iterations: each wait carries a timespec (IORING_ENTER_EXT_ARG,
        // kernel 5.11+), because a plain `submit_and_wait(1)` blocks forever
        // when an owed CQE never arrives — e.g. a pre-5.19 kernel rejecting
        // ASYNC_CANCEL_ANY with an -EINVAL CQE that the user_data==0 reap
        // branch cannot distinguish from success. On timeout exhaustion we
        // leak the remaining task boxes deliberately — a leak is recoverable,
        // freeing buffers the kernel still owns is not. (Pre-5.11 kernels fail
        // the EXT_ARG enter outright and fall straight through to the leak.)
        let wait_timeout = types::Timespec::new().nsec(10_000_000);
        let mut wait_attempts = 0;
        let max_wait_attempts = self.kernel_owed() * 4 + 64;
        while self.kernel_owed() > 0 && wait_attempts < max_wait_attempts {
            let submit_args = types::SubmitArgs::new().timespec(&wait_timeout);
            match self.io_uring.submitter().submit_with_args(1, &submit_args) {
                Ok(_) => {}
                Err(ref error) if error.raw_os_error() == Some(libc::ETIME) => {}
                Err(ref error) if is_transient_submit_error(error) => {}
                Err(_) => break,
            }
            self.reap_completions();
            wait_attempts += 1;
        }

        if self.kernel_owed() > 0 {
            eprintln!(
                "UringIOManager::drop: leaking {} in-flight operation(s) the kernel did not release",
                self.kernel_owed()
            );
        }

        // The kernel has finished with every reaped operation; dropping the
        // task boxes (and with them the I/O buffers) is now safe. Operations
        // the kernel never released keep their boxes alive forever via the
        // self-reference — the deliberate leak announced above.
        self.completed.clear();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cooperative_io::io_wait_future::IOWaitFuture;
    use crate::reactor::test_support::test_future_runtime;
    use io_uring::{opcode, types};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn nop_io_wait_future() -> Box<IOWaitFuture> {
        Box::new(IOWaitFuture::new(opcode::Nop::new().build(), &None))
    }

    /// Floods a tiny ring (SQ=2, CQ=4) with 16 Nops registered back-to-back
    /// without reaping in between: deterministically exercises the
    /// ring-full/EBUSY staging paths that used to be the #54 UAF and the #60
    /// accounting hole, then asserts every operation is delivered exactly once.
    #[test]
    fn nop_flood_on_tiny_ring_delivers_every_operation() {
        // Declaration order matters: `manager` is declared LAST so it drops
        // FIRST (locals drop in reverse order) — its Drop drain dereferences
        // the IOWaitFutures, which must still be alive at that point, on every
        // exit path including assertion-failure unwinds.
        let mut io_wait_futures: Vec<Box<IOWaitFuture>> = Vec::new();
        let mut drop_flags: Vec<Arc<AtomicBool>> = Vec::new();
        let mut manager = UringIOManager::try_new(2).unwrap();

        for _ in 0..16 {
            let mut io_wait_future = nop_io_wait_future();

            let drop_flag = Arc::new(AtomicBool::new(false));
            io_wait_future.set_future_runtime(test_future_runtime(drop_flag.clone()));
            drop_flags.push(drop_flag);

            let ptr: *const IOWaitFuture = &*io_wait_future;
            manager
                .register_io_wait(ptr)
                .expect("register must stage or fail before pushing");

            io_wait_futures.push(io_wait_future);
        }

        let mut delivered = 0;
        let mut spins = 0;
        while manager.has_active_io() {
            if manager.completed_io().is_some() {
                delivered += 1;
            } else {
                spins += 1;
                assert!(spins < 1_000_000, "completions stopped arriving");
            }
        }

        assert_eq!(delivered, 16);
        assert!(!manager.has_active_io());

        // Delivered runtimes were dropped by this loop (we discarded them).
        for drop_flag in &drop_flags {
            assert!(drop_flag.load(Ordering::Relaxed));
        }
    }

    /// Registers a read on a pipe that never receives data, then drops the
    /// manager: Drop must cancel the operation, reap the CQE, and free the
    /// parked task box (the former #58 teardown leak).
    #[test]
    fn drop_with_inflight_op_cancels_and_frees_the_task() {
        let mut pipe_fds = [0i32; 2];
        // SAFETY: valid out-array; fds closed below.
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);

        let mut buffer = [0u8; 8];
        let mut io_wait_future = Box::new(IOWaitFuture::new(
            opcode::Read::new(
                types::Fd(pipe_fds[0]),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
            .build(),
            &None,
        ));

        let drop_flag = Arc::new(AtomicBool::new(false));
        io_wait_future.set_future_runtime(test_future_runtime(drop_flag.clone()));

        {
            let mut manager = UringIOManager::try_new(8).unwrap();
            let ptr: *const IOWaitFuture = &*io_wait_future;
            manager.register_io_wait(ptr).unwrap();

            assert!(manager.has_active_io());
            assert!(!drop_flag.load(Ordering::Relaxed));

            // Manager dropped here with the read still in flight.
        }

        assert!(
            drop_flag.load(Ordering::Relaxed),
            "Drop must cancel the in-flight op and free its task box"
        );

        // SAFETY: fds were created by pipe() above and not closed elsewhere.
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }
    }

    /// Cancels an in-flight pipe read through the public cancel path and
    /// asserts the ECANCELED completion is delivered as TimedOut and the
    /// cancel CQE's accounting slot is released.
    #[test]
    fn cancel_io_wait_delivers_timed_out_and_releases_accounting() {
        let mut pipe_fds = [0i32; 2];
        // SAFETY: valid out-array; fds closed below.
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);

        let mut buffer = [0u8; 8];
        let mut io_wait_future = Box::new(IOWaitFuture::new(
            opcode::Read::new(
                types::Fd(pipe_fds[0]),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
            .build(),
            &None,
        ));

        let drop_flag = Arc::new(AtomicBool::new(false));
        io_wait_future.set_future_runtime(test_future_runtime(drop_flag.clone()));

        let mut manager = UringIOManager::try_new(8).unwrap();
        let ptr: *const IOWaitFuture = &*io_wait_future;
        manager.register_io_wait(ptr).unwrap();

        manager.cancel_io_wait(ptr).unwrap();

        let mut delivery = None;
        let mut spins = 0;
        while delivery.is_none() {
            delivery = manager.completed_io();
            spins += 1;
            assert!(spins < 10_000_000, "cancelled op never completed");
        }

        let (future_runtime, delivered_ptr) = delivery.unwrap();
        assert_eq!(delivered_ptr, ptr);
        drop(future_runtime);

        // Drain the cancel CQE's accounting slot.
        let mut spins = 0;
        while manager.has_active_io() {
            assert!(manager.completed_io().is_none());
            spins += 1;
            assert!(spins < 10_000_000, "cancel CQE never reaped");
        }

        let io_result = io_wait_future.take_io_result_for_test();
        assert_eq!(
            io_result.unwrap_err().kind(),
            io::ErrorKind::TimedOut,
            "ECANCELED must surface as TimedOut"
        );

        // SAFETY: fds were created by pipe() above and not closed elsewhere.
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }
    }
}
