//! kqueue-based I/O manager for the reactor.
//!
//! Implements `IOManager` using macOS kqueue to monitor file descriptors for readiness,
//! driving cooperative future completion for file and network I/O operations.
//!
//! # Invariants
//!
//! 1. **A reaped event is a completion or a wake — never discardable.** I/O
//!    events are registered edge-triggered (`EV_CLEAR`) and I/O timeouts are
//!    `EV_ONESHOT` timers, so the reap is the only notification kqueue gives.
//!    Real events reaped by `wait_for_io`'s blocking `kevent` are buffered in
//!    `buffered_events` for `completed_io` to deliver; dropping one would
//!    strand its parked task forever.
//! 2. **The wake channel is not I/O.** The `EVFILT_USER` event
//!    (`WAKEUP_IDENT`, registered `EV_CLEAR` at construction) is never counted
//!    in `active_io` — it can neither keep the reactor loop alive nor be
//!    delivered as a completion. Both reap sites classify it by
//!    ident+filter and consume it: interpreting it as I/O would dereference
//!    its null `udata`. `wake()` posts `NOTE_TRIGGER` from any thread
//!    (kqueue changelists are thread-safe); triggers coalesce until the next
//!    reap, where `EV_CLEAR` auto-resets the event.
//! 3. **A `kevent()` error yields no events.** On `-1` the eventlist is
//!    meaningless; error paths return without interpreting it (a
//!    stack-initialized struct dereferenced through `udata` was former
//!    tracker #45).
//! 4. **Sleeping is interruptible** (mirror of the Linux manager's
//!    invariant 4): if the `EVFILT_USER` registration failed at construction,
//!    `wait_for_io` caps every sleep at `MAX_UNWAKEABLE_SLEEP` so reactor
//!    liveness never depends on the wake channel.
//! 5. **Delivery purges buffered siblings.** A timed I/O wait owns two
//!    knotes — the readiness event and an `EV_ONESHOT` `EVFILT_TIMER`, both
//!    stamping the same `IOWaitFuture` into `udata` — and `wait_for_io`'s
//!    batch reap can copy BOTH out of the kernel in one call (data arriving
//!    as the timeout expires). The kernel-side `EV_DELETE` issued at
//!    delivery retracts the sibling's pending activation only while it is
//!    still in the kqueue; it cannot reach a copy already reaped into
//!    `buffered_events`. `completed_io` therefore scrubs the delivered
//!    future's remaining buffered events — a surviving sibling would be
//!    delivered again after the task (and its `IOWaitFuture`) is freed: a
//!    use-after-free plus a stolen `active_io` decrement.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::raw::c_void;
use std::ptr;
use std::time::{Duration, SystemTime};

use kqueue_sys::{EventFilter, EventFlag, FilterFlag, kevent, kqueue};

use crate::cooperative_io::io_manager::{
    IOManager, IOManagerCreateOptions, IOManagerHolder, WaitResult,
};
use crate::cooperative_io::io_timeout::IOTimeout;
use crate::reactor::FutureRuntime;

use super::io_wait_future::IOWaitFuture;

/// `ident` of the cross-thread wake event (module invariant 2). `EVFILT_USER`
/// idents are a private per-kqueue namespace, so this cannot collide with
/// real registrations (fd numbers and `IOWaitFuture` addresses).
const WAKEUP_IDENT: usize = usize::MAX;

/// `NOTE_TRIGGER` from `<sys/event.h>`: fires a registered `EVFILT_USER`
/// event. Missing from kqueue-sys 1.0.4's darwin constants, hence the raw
/// bit; `from_bits_unchecked` is "unsafe" only as an API contract (the bit is
/// real, just outside the crate's defined set).
const NOTE_TRIGGER: FilterFlag = unsafe { FilterFlag::from_bits_unchecked(0x0100_0000) };

/// Max events reaped per blocking `kevent` call in `wait_for_io`; leftovers
/// stay pending in the kqueue and surface on the next reap.
const WAIT_EVENT_BATCH: usize = 16;

/// Sleep cap when the wake channel is unavailable (module invariant 4).
const MAX_UNWAKEABLE_SLEEP: Duration = Duration::from_millis(1);

/// True for reaps of the `EVFILT_USER` wake event (module invariant 2).
fn is_wake_event(k_event: &kevent) -> bool {
    k_event.filter == EventFilter::EVFILT_USER && k_event.ident == WAKEUP_IDENT
}

pub(crate) struct KqueueIOManager {
    kqueue_file: File,
    active_io: usize,

    /// Real I/O events reaped by `wait_for_io`'s blocking `kevent` but not
    /// yet delivered; `completed_io` drains this before polling the kqueue
    /// (module invariant 1).
    buffered_events: VecDeque<kevent>,

    /// Whether the `EVFILT_USER` wake event was registered at construction.
    /// When false, `wake()` is a no-op and `wait_for_io` bounds every sleep
    /// (module invariant 4).
    wake_registered: bool,
}

impl KqueueIOManager {
    pub(crate) fn try_new() -> io::Result<Self> {
        let kqueue_fd = unsafe { kqueue() };

        if kqueue_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let kqueue_file = unsafe { File::from_raw_fd(kqueue_fd) };

        // Register the wake channel (module invariant 2). `EV_CLEAR` makes
        // the event auto-reset when reaped, coalescing triggers in between.
        // Registration failure degrades to bounded sleeps (invariant 4)
        // instead of failing reactor construction.
        let wake_event = kevent {
            ident: WAKEUP_IDENT,
            filter: EventFilter::EVFILT_USER,
            flags: EventFlag::EV_ADD | EventFlag::EV_CLEAR,
            fflags: FilterFlag::empty(),
            data: 0,
            udata: ptr::null_mut(),
        };

        // SAFETY: valid kq fd; the changelist points at one initialized
        // kevent; nevents = 0 means the eventlist is unused and the call
        // cannot block.
        let result = unsafe {
            kevent(
                kqueue_file.as_raw_fd(),
                &wake_event,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };

        Ok(KqueueIOManager {
            kqueue_file,
            active_io: 0,
            buffered_events: VecDeque::new(),
            wake_registered: result != -1,
        })
    }

    /// Next deliverable event: drains `buffered_events` (filled by
    /// `wait_for_io` — module invariant 1) before polling the kqueue without
    /// blocking. Returns `None` when nothing is pending or `kevent()` fails
    /// (module invariant 3: an errored eventlist must not be interpreted).
    fn next_event(&mut self) -> Option<kevent> {
        if let Some(k_event) = self.buffered_events.pop_front() {
            return Some(k_event);
        }

        let mut k_event = kevent::new(
            0,
            EventFilter::EVFILT_SYSCOUNT,
            EventFlag::empty(),
            FilterFlag::empty(),
        );

        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };

        // SAFETY: valid kq fd; the eventlist points at one writable kevent;
        // the zero timeout makes this a non-blocking poll.
        let event_count = unsafe {
            kevent(
                self.kqueue_file.as_raw_fd(),
                ptr::null(),
                0,
                &mut k_event,
                1,
                &timeout,
            )
        };

        if event_count <= 0 {
            return None;
        }

        Some(k_event)
    }

    /// Drops buffered events that belong to the future being delivered
    /// (module invariant 5). Both of a timed wait's knotes stamp the future
    /// into `udata`, so address equality identifies every sibling; the wake
    /// event (null `udata`) is never buffered, so it cannot be falsely
    /// matched.
    fn purge_buffered_events(&mut self, io_wait_future_ptr: *const IOWaitFuture) {
        self.buffered_events
            .retain(|k_event| !ptr::eq(k_event.udata as *const IOWaitFuture, io_wait_future_ptr));
    }
}

impl IOManager for KqueueIOManager {
    fn has_active_io(&self) -> bool {
        self.active_io != 0
    }

    /// See [`IOManager::completed_io`].
    ///
    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)> {
        let k_event = loop {
            let k_event = self.next_event()?;

            // Stale wake (module invariant 2): wake() raced with a reactor
            // that was already awake, or its trigger outlived the sleep it
            // ended. Consume it and keep scanning — interpreting it as a
            // completion would dereference its null udata.
            if is_wake_event(&k_event) {
                continue;
            }

            break k_event;
        };

        // Timer event — the I/O operation timed out.
        //
        if k_event.filter == EventFilter::EVFILT_TIMER {
            let io_wait_future_ptr = k_event.udata as *mut IOWaitFuture;
            let io_wait_future = unsafe { &mut (*io_wait_future_ptr) };

            // Delete the I/O kevent.
            //
            let delete_io_event = kevent {
                ident: io_wait_future.k_event.ident,
                filter: io_wait_future.k_event.filter,
                fflags: FilterFlag::empty(),
                data: 0,
                udata: ptr::null_mut(),
                flags: EventFlag::EV_DELETE,
            };

            unsafe {
                kevent(
                    self.kqueue_file.as_raw_fd(),
                    &delete_io_event,
                    1,
                    ptr::null_mut(),
                    0,
                    ptr::null(),
                )
            };

            // Set timeout error on the IOWaitFuture.
            //
            io_wait_future.set_io_result(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "I/O operation timed out",
            )));

            self.active_io -= 1;
            self.purge_buffered_events(io_wait_future_ptr);

            let future_runtime = io_wait_future.take_future_runtime().unwrap();
            return Some((future_runtime, io_wait_future_ptr));
        }

        // Normal I/O completion.
        //
        let io_wait_future_ptr = k_event.udata as *mut IOWaitFuture;
        let io_wait_future = unsafe { &mut (*io_wait_future_ptr) };

        io_wait_future.handle_kevent();

        if !io_wait_future.is_completed() {
            return None;
        }

        // Unregister kevent after the IOWaitFuture has completed.
        //
        let delete_k_event = kevent {
            ident: k_event.ident,
            filter: k_event.filter,
            fflags: FilterFlag::empty(),
            data: 0,
            udata: ptr::null_mut(),
            flags: EventFlag::EV_DELETE,
        };

        let _result = unsafe {
            kevent(
                self.kqueue_file.as_raw_fd(),
                &delete_k_event,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };

        // Delete the timer kevent if one was registered.
        //
        let delete_timer = kevent {
            ident: io_wait_future_ptr as usize,
            filter: EventFilter::EVFILT_TIMER,
            fflags: FilterFlag::empty(),
            data: 0,
            udata: ptr::null_mut(),
            flags: EventFlag::EV_DELETE,
        };

        unsafe {
            kevent(
                self.kqueue_file.as_raw_fd(),
                &delete_timer,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };

        // TODO: handle kevent deletion failure.

        self.active_io -= 1;
        self.purge_buffered_events(io_wait_future_ptr);

        let future_runtime = io_wait_future.take_future_runtime().unwrap();

        Some((future_runtime, io_wait_future_ptr))
    }

    /// See [`IOManager::register_io_wait`].
    ///
    fn register_io_wait(
        &mut self,
        io_wait_future_ptr: *const IOWaitFuture,
    ) -> Result<(), std::io::Error> {
        // Store IOWaitFuture pointer in the Kevent struct.
        //
        let io_wait_future = unsafe { &mut (*(io_wait_future_ptr as *mut IOWaitFuture)) };
        io_wait_future.k_event.udata = ptr::addr_of!(*io_wait_future_ptr) as *mut c_void;

        let k_event_ptr: *const kevent = ptr::addr_of!(io_wait_future.k_event);

        // Register the I/O event.
        //
        let result = unsafe {
            kevent(
                self.kqueue_file.as_raw_fd(),
                k_event_ptr,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };

        if result == -1 {
            return Err(io::Error::last_os_error());
        }

        // If timeout specified, register a one-shot timer event.
        //
        if let Some(timeout) = io_wait_future.timeout() {
            let duration_ns = Self::timeout_to_nanos(timeout);

            let timer_event = kevent {
                ident: io_wait_future_ptr as usize,
                filter: EventFilter::EVFILT_TIMER,
                flags: EventFlag::EV_ADD | EventFlag::EV_ONESHOT,
                fflags: FilterFlag::NOTE_NSECONDS,
                data: duration_ns as i64,
                udata: io_wait_future_ptr as *mut c_void,
            };

            let result = unsafe {
                kevent(
                    self.kqueue_file.as_raw_fd(),
                    &timer_event,
                    1,
                    ptr::null_mut(),
                    0,
                    ptr::null(),
                )
            };

            if result == -1 {
                // Timer registration failed — clean up the I/O kevent.
                //
                let delete_k_event = kevent {
                    ident: io_wait_future.k_event.ident,
                    filter: io_wait_future.k_event.filter,
                    fflags: FilterFlag::empty(),
                    data: 0,
                    udata: ptr::null_mut(),
                    flags: EventFlag::EV_DELETE,
                };

                unsafe {
                    kevent(
                        self.kqueue_file.as_raw_fd(),
                        &delete_k_event,
                        1,
                        ptr::null_mut(),
                        0,
                        ptr::null(),
                    )
                };

                return Err(io::Error::last_os_error());
            }

            // Consume the timeout so the reactor does not also track it.
            //
            io_wait_future.set_timeout(None);
        }

        self.active_io += 1;
        Ok(())
    }

    /// See [`IOManager::cancel_io_wait`].
    ///
    fn cancel_io_wait(&mut self, _: *const IOWaitFuture) -> Result<(), io::Error> {
        Ok(())
    }

    /// See [`IOManager::wait_for_io`].
    ///
    /// Race-free against its completion source: kqueue makes event collection
    /// atomic with blocking, so events — including a wake triggered between
    /// the reactor's sleeping-flag handshake and this call — that arrived
    /// just before blocking are returned, never lost. Real events reaped here
    /// are buffered for `completed_io` (module invariant 1).
    fn wait_for_io(&mut self, timeout: Option<Duration>) -> WaitResult {
        if !self.buffered_events.is_empty() {
            return WaitResult::IoCompleted;
        }

        // Without the wake channel an unbounded sleep could never be
        // interrupted by wake(); bound it so liveness never depends on the
        // wake event (module invariant 4).
        let effective_timeout = if self.wake_registered {
            timeout
        } else {
            Some(
                timeout
                    .unwrap_or(MAX_UNWAKEABLE_SLEEP)
                    .min(MAX_UNWAKEABLE_SLEEP),
            )
        };

        let timeout_spec = effective_timeout.map(|duration| libc::timespec {
            // Saturate far-future deadlines instead of overflowing time_t.
            tv_sec: duration.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
            tv_nsec: duration.subsec_nanos() as i64,
        });

        // NULL = block until an event or a wake arrives.
        let timeout_ptr = timeout_spec
            .as_ref()
            .map_or(ptr::null(), |timespec| ptr::addr_of!(*timespec));

        let mut events = [kevent::new(
            0,
            EventFilter::EVFILT_SYSCOUNT,
            EventFlag::empty(),
            FilterFlag::empty(),
        ); WAIT_EVENT_BATCH];

        // SAFETY: valid kq fd; the eventlist points at WAIT_EVENT_BATCH
        // writable kevents; timeout_ptr is null or points at a timespec that
        // outlives the call.
        let event_count = unsafe {
            kevent(
                self.kqueue_file.as_raw_fd(),
                ptr::null(),
                0,
                events.as_mut_ptr(),
                WAIT_EVENT_BATCH as i32,
                timeout_ptr,
            )
        };

        if event_count < 0 {
            // The eventlist must not be interpreted on error (module
            // invariant 3). EINTR ends the sleep like a wake; either way the
            // result is advisory and the reactor re-scans its queues.
            return if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                WaitResult::Woken
            } else {
                WaitResult::TimedOut
            };
        }

        let mut woken = false;
        for k_event in &events[..event_count as usize] {
            if is_wake_event(k_event) {
                // Consumed by this reap; EV_CLEAR already reset the event
                // (module invariant 2).
                woken = true;
            } else {
                self.buffered_events.push_back(*k_event);
            }
        }

        if !self.buffered_events.is_empty() {
            WaitResult::IoCompleted
        } else if woken {
            WaitResult::Woken
        } else {
            WaitResult::TimedOut
        }
    }

    /// See [`IOManager::wake`]. Thread-safe: posting a kqueue changelist
    /// entry is safe from any thread by the kqueue contract, and no manager
    /// state is touched. `NOTE_TRIGGER` fires the registered `EVFILT_USER`
    /// event; triggers posted before the sleeper reaps coalesce into one
    /// delivery (module invariant 2).
    fn wake(&self) {
        if !self.wake_registered {
            // Degraded mode (module invariant 4): sleeps are bounded by
            // MAX_UNWAKEABLE_SLEEP, so the reactor notices new work without
            // a wake.
            return;
        }

        let trigger_event = kevent {
            ident: WAKEUP_IDENT,
            filter: EventFilter::EVFILT_USER,
            flags: EventFlag::empty(),
            fflags: NOTE_TRIGGER,
            data: 0,
            udata: ptr::null_mut(),
        };

        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };

        // SAFETY: valid kq fd for the manager's lifetime; one initialized
        // changelist entry; nevents = 0 means the eventlist is unused and
        // the call cannot block.
        unsafe {
            kevent(
                self.kqueue_file.as_raw_fd(),
                &trigger_event,
                1,
                ptr::null_mut(),
                0,
                &timeout,
            )
        };
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any
    where
        Self: 'static,
    {
        self
    }
}

impl KqueueIOManager {
    /// Converts an IOTimeout to nanoseconds from now.
    ///
    fn timeout_to_nanos(timeout: &IOTimeout) -> u64 {
        let duration = match timeout {
            IOTimeout::Duration(d) => *d,
            IOTimeout::AbsoluteTime(t) => t
                .duration_since(SystemTime::now())
                .unwrap_or(std::time::Duration::ZERO),
        };

        duration.as_nanos() as u64
    }
}

// Implement reactor create options.
//
pub struct KqueueIOManagerReactorCreateOptions {}

impl Default for KqueueIOManagerReactorCreateOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl KqueueIOManagerReactorCreateOptions {
    pub fn new() -> Self {
        KqueueIOManagerReactorCreateOptions {}
    }
}

impl IOManagerCreateOptions for KqueueIOManagerReactorCreateOptions {
    fn try_new_io_manager(&self) -> io::Result<IOManagerHolder> {
        Ok(IOManagerHolder {
            io_manager: Box::new(KqueueIOManager::try_new()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Instant;

    use super::*;
    use crate::cooperative_io::io_macos::io_wait_future::IOCallbackWrapper;
    use crate::cooperative_io::io_timeout::IOTimeout;
    use crate::reactor::{FutureRuntimeTrait, Reactor};

    /// Minimal runtime so `take_future_runtime().unwrap()` has something to
    /// deliver; the tests drive the manager directly, no reactor involved.
    struct StubRuntime;

    impl FutureRuntimeTrait for StubRuntime {
        fn poll_future(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            Poll::Ready(())
        }
        fn is_first_yield(&self) -> bool {
            false
        }
        fn set_first_yield(&mut self, _: bool) {}
        fn increment_execution_count(&mut self) {}
        fn signal_completed(&self) {}
        fn assigned_reactor(&self) -> Option<&Reactor> {
            None
        }
    }

    /// RAII pipe; `read_fd`/`write_fd` stay open for the struct's lifetime.
    struct Pipe {
        read: File,
        write: File,
    }

    impl Pipe {
        fn new() -> Self {
            let mut fds = [0i32; 2];
            // SAFETY: fds is a valid out-array of two c_ints.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            // SAFETY: fresh fds exclusively owned by the two Files.
            unsafe {
                Pipe {
                    read: File::from_raw_fd(fds[0]),
                    write: File::from_raw_fd(fds[1]),
                }
            }
        }

        fn write_byte(&self) {
            // SAFETY: valid write end; 1-byte write from a live buffer.
            assert_eq!(
                unsafe { libc::write(self.write.as_raw_fd(), b"x".as_ptr().cast(), 1) },
                1
            );
        }
    }

    /// An `IOWaitFuture` watching `read_fd` for readability, registered the
    /// way `io_network`/`io_file` register theirs (`EV_ADD|EV_ENABLE|EV_CLEAR`).
    /// The callback completes on first invocation (`should_continue = false`).
    fn new_read_wait(read_fd: i32, buffer: &[u8], timeout: Option<IOTimeout>) -> IOWaitFuture {
        let k_event = kevent {
            ident: read_fd as usize,
            filter: EventFilter::EVFILT_READ,
            flags: EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR,
            fflags: FilterFlag::empty(),
            data: 0,
            udata: ptr::null_mut(),
        };

        let mut io_wait_future = IOWaitFuture::new(
            k_event,
            buffer as *const [u8],
            false,
            &timeout,
            IOCallbackWrapper {
                callback: |_: &kevent, _: &mut [u8]| Ok(1),
            },
        );
        io_wait_future.set_future_runtime(Box::pin(StubRuntime));

        io_wait_future
    }

    /// Module invariant 5 regression: data arriving as the timeout expires
    /// puts BOTH of the wait's knotes (readiness + one-shot timer) into one
    /// blocking batch reap. Delivering one must purge the buffered sibling —
    /// pre-purge, the second `completed_io` re-delivered the same future
    /// (use-after-free once the task is freed; here, an unwrap panic).
    #[test]
    fn delivery_purges_buffered_sibling_events() {
        let mut manager = KqueueIOManager::try_new().unwrap();
        let pipe = Pipe::new();
        let buffer = [0u8; 8];

        let io_wait_future = new_read_wait(
            pipe.read.as_raw_fd(),
            &buffer,
            Some(IOTimeout::from_duration(Duration::from_millis(1))),
        );
        let io_wait_future_ptr = ptr::addr_of!(io_wait_future);

        manager.register_io_wait(io_wait_future_ptr).unwrap();
        assert!(manager.has_active_io());

        // Make BOTH knotes active before the reap: readable now, timer fired.
        pipe.write_byte();
        std::thread::sleep(Duration::from_millis(10));

        assert_eq!(
            manager.wait_for_io(Some(Duration::from_millis(100))),
            WaitResult::IoCompleted
        );
        assert_eq!(
            manager.buffered_events.len(),
            2,
            "batch reap should have captured both sibling events"
        );

        let (_runtime, delivered_ptr) = manager.completed_io().expect("one delivery");
        assert_eq!(delivered_ptr, io_wait_future_ptr);
        assert!(
            manager.buffered_events.is_empty(),
            "delivery must purge the buffered sibling (module invariant 5)"
        );
        assert!(!manager.has_active_io());

        assert!(
            manager.completed_io().is_none(),
            "the purged sibling must never be delivered"
        );
    }

    /// A wake that lands while the reactor is awake stays pending in the
    /// kqueue until `completed_io`'s poll reaps it; it must be skipped, not
    /// interpreted as a completion (its `udata` is null — module invariant 2).
    #[test]
    fn completed_io_skips_stale_wake_events() {
        let mut manager = KqueueIOManager::try_new().unwrap();
        let pipe = Pipe::new();
        let buffer = [0u8; 8];

        // A registered-but-quiet wait keeps active_io > 0 so the scenario
        // matches the reactor's gate for calling completed_io at all.
        let io_wait_future = new_read_wait(pipe.read.as_raw_fd(), &buffer, None);
        manager
            .register_io_wait(ptr::addr_of!(io_wait_future))
            .unwrap();

        manager.wake();

        assert!(manager.completed_io().is_none());
        assert!(
            manager.has_active_io(),
            "the wake must not consume real I/O"
        );
    }

    /// A wake posted before the sleep must end it immediately — kqueue's
    /// atomic collect-pending-then-block is what closes the reactor's
    /// store-flag/recheck handshake against lost wakeups.
    #[test]
    fn wake_before_wait_prevents_blocking() {
        let mut manager = KqueueIOManager::try_new().unwrap();

        manager.wake();

        let start = Instant::now();
        let result = manager.wait_for_io(Some(Duration::from_secs(5)));
        assert_eq!(result, WaitResult::Woken);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "pre-posted wake was lost: wait blocked for the full timeout"
        );
    }

    /// With nothing pending the wait must block for the requested bound and
    /// report the timeout.
    #[test]
    fn wait_times_out_when_idle() {
        let mut manager = KqueueIOManager::try_new().unwrap();

        let start = Instant::now();
        let result = manager.wait_for_io(Some(Duration::from_millis(30)));

        assert_eq!(result, WaitResult::TimedOut);
        assert!(start.elapsed() >= Duration::from_millis(25));
    }
}
