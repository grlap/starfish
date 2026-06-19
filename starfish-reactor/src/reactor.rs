//! Core reactor implementing cooperative scheduling of futures.
//!
//! Provides the `Reactor` struct, which drives futures to completion on a single thread,
//! and `FutureRuntimeTrait` / `ScheduleReason` for managing task lifecycle and wake-up
//! scheduling within the cooperative runtime. External task spawning (`spawn_external`)
//! supports both cooperative completion (when called from a reactor thread) and preemptive
//! completion via `CountdownEvent` (when called from a non-reactor thread).

use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::SystemTime;
use std::{io, ptr};

use binary_heap_plus::{BinaryHeap, MinComparator};
use crossbeam::queue::SegQueue;

use crate::completion_signaler::{CompletionSignaler, ExternalCompletionWaiter};
use crate::cooperative_io::io_manager::{IOManager, IOManagerCreateOptions, IOManagerHolder};
use crate::cooperative_io::io_wait_future::IOWaitFuture;
use crate::cooperative_synchronization::event_future::{CooperativeEventFuture, EventFuture};
use crate::cooperative_synchronization::lock::CooperativeFairLock;
use crate::cooperative_synchronization::wait_one_future::CooperativeWaitOneFuture;
use crate::cooperative_synchronization::wait_one_future::CooperativeWaitOneSignaler;
use crate::rc_pointer::RcPointer;
use starfish_core::data_structures::Treap;
use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;

pub trait ReactorAssigned {
    fn assigned_reactor(&self) -> Option<&Reactor>;
}

// Defines a future scheduling reason.
//
#[derive(Debug)]
pub(crate) enum ScheduleReason {
    None,
    Pending,
    Yield,
    EventWait(*const EventFuture),
    WaitOne(*const CooperativeWaitOneFuture),
    IOWait(*const IOWaitFuture),
    DelayedWait { activate_system_time: SystemTime },
}

// Trait for dynamic dispatch over generic FutureRuntime.
// This allows storing different future types in the same queue with a single allocation.
//
pub(crate) trait FutureRuntimeTrait {
    /// Poll the underlying future. Must be called on a pinned reference.
    ///
    fn poll_future(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()>;

    fn is_first_yield(&self) -> bool;
    fn set_first_yield(&mut self, val: bool);
    fn increment_execution_count(&mut self);
    fn signal_completed(&self);
    fn assigned_reactor(&self) -> Option<&Reactor>;
}

// Generic future runtime that stores the future inline (no separate Box allocation).
// Uses either cooperative or preemptive completion signaling.
//
pub struct FutureRuntimeImpl<F: Future<Output = ()>> {
    future: F,
    is_first_yield: bool,
    execution_count: u32,
    completed_signaler: CompletionSignaler,
}

impl<F: Future<Output = ()>> FutureRuntimeImpl<F> {
    fn new(future: F, completed_signaler: CompletionSignaler) -> Self {
        FutureRuntimeImpl {
            future,
            is_first_yield: true,
            execution_count: 0,
            completed_signaler,
        }
    }
}

impl<F: Future<Output = ()>> FutureRuntimeTrait for FutureRuntimeImpl<F> {
    fn poll_future(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: We're projecting from Pin<&mut Self> to Pin<&mut F>.
        // This is safe because:
        // 1. FutureRuntimeImpl is !Unpin (contains F which may be !Unpin)
        // 2. We never move F after pinning
        // 3. The future field is structurally pinned
        //
        let future = unsafe { self.map_unchecked_mut(|s| &mut s.future) };
        future.poll(cx)
    }

    fn is_first_yield(&self) -> bool {
        self.is_first_yield
    }

    fn set_first_yield(&mut self, val: bool) {
        self.is_first_yield = val;
    }

    fn increment_execution_count(&mut self) {
        self.execution_count += 1;
    }

    fn signal_completed(&self) {
        self.completed_signaler.signal();
    }

    fn assigned_reactor(&self) -> Option<&Reactor> {
        self.completed_signaler.assigned_reactor()
    }
}

// Type alias for boxed future runtime trait object.
//
pub(crate) type FutureRuntime = Pin<Box<dyn FutureRuntimeTrait>>;

impl ReactorAssigned for dyn FutureRuntimeTrait {
    fn assigned_reactor(&self) -> Option<&Reactor> {
        FutureRuntimeTrait::assigned_reactor(self)
    }
}

pub struct ScheduledFutureRuntime {
    activate_system_time: SystemTime,
    future_runtime: Option<FutureRuntime>,
}

impl ScheduledFutureRuntime {
    fn new(activate_system_time: SystemTime, future_runtime: FutureRuntime) -> Self {
        ScheduledFutureRuntime {
            activate_system_time,
            future_runtime: Some(future_runtime),
        }
    }
}

impl Ord for ScheduledFutureRuntime {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.activate_system_time.cmp(&other.activate_system_time)
    }
}

impl Eq for ScheduledFutureRuntime {}

impl PartialOrd for ScheduledFutureRuntime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ScheduledFutureRuntime {
    fn eq(&self, other: &Self) -> bool {
        self.activate_system_time == other.activate_system_time
    }
}

/// A shared result slot for passing a value from a producer future to a consumer future.
///
/// # Safety
/// `Send + Sync` is sound because access is sequenced by a completion event:
/// the producer writes before signaling, the consumer reads only after the event fires.
/// There is no concurrent access to the inner `UnsafeCell`.
struct ResultExternalFuture<T>(UnsafeCell<Option<T>>);

unsafe impl<T: Send> Send for ResultExternalFuture<T> {}
unsafe impl<T: Send> Sync for ResultExternalFuture<T> {}

impl<T> ResultExternalFuture<T> {
    fn new() -> Self {
        ResultExternalFuture(UnsafeCell::new(None))
    }

    /// # Safety
    /// Must only be called by the producer, before signaling completion.
    unsafe fn set(&self, value: T) {
        unsafe { *self.0.get() = Some(value) }
    }

    /// # Safety
    /// Must only be called by the consumer, after the completion event fires.
    unsafe fn take(&self) -> T {
        unsafe { (*self.0.get()).take().unwrap() }
    }
}

// Defines a Cooperative Reactor.
//
pub struct Reactor {
    active_futures: RefCell<VecDeque<FutureRuntime>>,
    delayed_futures: BinaryHeap<ScheduledFutureRuntime, MinComparator>,
    external_futures: SegQueue<FutureRuntime>,
    schedule_reason: ScheduleReason,
    io_futures_with_timeout: Treap<*const IOWaitFuture, *const IOWaitFuture, SystemTime>,
    yield_count: u32,
    io_manager: Box<dyn IOManager>,
    index: usize,
    is_in_shutdown: AtomicBool,
    /// Consecutive external futures executed; bounds external priority so a
    /// sustained cross-reactor feed cannot starve I/O completion delivery.
    external_streak: u32,
    /// Consecutive loop iterations that found nothing runnable; gates the
    /// idle-spin window before the reactor blocks in `wait_for_io`.
    idle_streak: u32,
    /// True while this reactor is (about to be) blocked in `wait_for_io`.
    /// Senders check it after pushing work and call `IOManager::wake()`.
    /// The store-buffering handshake (SeqCst fences on both sides — see
    /// `run()` and `enqueue_external_future_runtime`) is what makes a lost
    /// wakeup impossible; plain Release/Acquire would not. Cache-padded:
    /// senders read it on every external enqueue, and without padding it
    /// would share a line with the reactor's hot private counters
    /// (idle_streak/external_streak), turning every idle-loop write into
    /// cross-core traffic.
    sleeping: crossbeam::utils::CachePadded<AtomicBool>,
}

/// After this many consecutive external picks, I/O completions get one turn.
/// Externals keep ~STREAK/(STREAK+1) of the reactor under sustained pressure.
const EXTERNAL_PRIORITY_STREAK: u32 = 8;

/// Idle loop iterations before the reactor blocks in the kernel. Each idle
/// iteration is a few hundred nanoseconds of queue checks, so this absorbs
/// transient lulls of tens of microseconds without paying the sleep/wake
/// syscall round-trip (Seastar's "idle polling" phase).
const IDLE_SPIN_ITERATIONS: u32 = 256;

impl Reactor {
    pub fn new() -> Self {
        Self::try_new().unwrap()
    }

    pub fn try_new() -> io::Result<Self> {
        Ok(Self::with_io_manager(
            crate::cooperative_io::DefaultIOManagerCreateOptions::default().try_new_io_manager()?,
        ))
    }

    pub fn try_new_with_io_manager<T: IOManagerCreateOptions + Send + Sync + 'static>(
        io_manager_create_options: T,
    ) -> io::Result<Self> {
        let io_manager = io_manager_create_options.try_new_io_manager()?;

        Ok(Self::with_io_manager(io_manager))
    }

    pub(crate) fn with_io_manager(io_manager_holder: IOManagerHolder) -> Self {
        Reactor {
            active_futures: RefCell::new(VecDeque::default()),
            delayed_futures: BinaryHeap::new_min(),
            external_futures: SegQueue::new(),
            schedule_reason: ScheduleReason::None,
            io_futures_with_timeout: Treap::new(),
            yield_count: 0,
            io_manager: io_manager_holder.io_manager,
            index: 0,
            is_in_shutdown: AtomicBool::new(true),
            external_streak: 0,
            idle_streak: 0,
            sleeping: crossbeam::utils::CachePadded::new(AtomicBool::new(false)),
        }
    }

    /// Sets the scheduling reason for a given future.
    ///
    #[inline]
    pub(super) fn set_schedule_reason(&mut self, reason: ScheduleReason) {
        self.schedule_reason = reason;
    }

    /// Takes the next completed I/O future from the I/O manager, dropping its
    /// entry from the timeout collection if it carried a timeout.
    ///
    fn take_completed_io(&mut self) -> Option<FutureRuntime> {
        if !self.io_manager.has_active_io() {
            return None;
        }

        let (future_runtime, io_wait_future_ptr) = self.io_manager.completed_io()?;

        // SAFETY: the manager only delivers pointers to IOWaitFutures whose
        // owning task box it just handed back in `future_runtime` — the
        // pointee is alive until that runtime is dropped or completed.
        let io_wait_future = unsafe { &*io_wait_future_ptr };

        if io_wait_future.timeout().is_some() {
            // If completed IOWaitFuture has timeout, remove the future from io_futures_with_timeout.
            //
            let const_ptr: *const IOWaitFuture = io_wait_future_ptr;
            self.io_futures_with_timeout.remove(&const_ptr);
        }

        Some(future_runtime)
    }

    // Main event loop for the reactor.
    // Executes active features.
    //
    pub fn run(&mut self) {
        // Set reactor local thread instance.
        //
        Reactor::set_local_instance(self);

        let waker = create_waker(self);

        let mut context = Context::from_waker(&waker);

        let mut should_run = true;

        while should_run {
            // Cancel timed-out IOWaitFutures.
            //
            while let Some((key, _, timeout)) = self.io_futures_with_timeout.get_min_priority_node()
            {
                if *timeout <= SystemTime::now() {
                    // IOWaitFuture expired. Cancel outstanding IO, ignore any errors.
                    //
                    let io_wait_future_ptr =
                        self.io_futures_with_timeout.remove(&key.clone()).unwrap();

                    _ = self.io_manager.cancel_io_wait(io_wait_future_ptr)
                } else {
                    // Earliest IOWaitFuture is not expired.
                    //
                    break;
                }
            }

            // External futures run first and directly: a cross-reactor spawn
            // has another CPU blocked on the handoff, so every queue hop is
            // cross-core latency. The priority is bounded by a streak valve —
            // after EXTERNAL_PRIORITY_STREAK consecutive external picks, I/O
            // completions get one turn — so a sustained external feed cannot
            // strand in-flight I/O forever (including the cancellation
            // completion that resumes a timed-out task).
            //
            let give_io_turn = self.external_streak >= EXTERNAL_PRIORITY_STREAK;

            let mut option_future_runtime = if give_io_turn {
                self.take_completed_io()
            } else {
                None
            };

            if option_future_runtime.is_some() {
                self.external_streak = 0;
            } else {
                option_future_runtime = self.external_futures.pop();

                if option_future_runtime.is_some() {
                    // Saturating: only the >= EXTERNAL_PRIORITY_STREAK
                    // comparison matters, and a sustained feed with no I/O
                    // active would otherwise overflow the counter.
                    self.external_streak = self.external_streak.saturating_add(1);
                } else {
                    self.external_streak = 0;

                    if !give_io_turn {
                        option_future_runtime = self.take_completed_io();
                    }
                }
            }

            // Handle timers.
            //
            if option_future_runtime.is_none() && !self.delayed_futures.is_empty() {
                let now = SystemTime::now();

                if let Some(delayed_future) = self.delayed_futures.peek()
                    && now >= delayed_future.activate_system_time
                {
                    option_future_runtime =
                        self.delayed_futures.pop().unwrap().future_runtime.take();
                }
            }

            if option_future_runtime.is_none() {
                option_future_runtime = self.active_futures.borrow_mut().pop_front();
            }

            if option_future_runtime.is_none() {
                should_run = !self
                    .is_in_shutdown
                    .load(std::sync::atomic::Ordering::Acquire)
                    || self.io_manager.has_active_io()
                    || !self.external_futures.is_empty()
                    || !self.delayed_futures.is_empty();

                if !should_run {
                    continue;
                }

                // Idle phases (former trackers #5/#59): spin briefly for
                // latency, then block in the kernel instead of burning the
                // core.
                //
                // Phase 1 — spin window: absorb transient lulls without the
                // sleep/wake syscall round-trip.
                //
                self.idle_streak = self.idle_streak.saturating_add(1);
                if self.idle_streak < IDLE_SPIN_ITERATIONS {
                    continue;
                }

                // Phase 2 — bound the sleep by the earliest deadline from
                // EITHER deadline source: delayed futures (timers) or the
                // I/O-timeout Treap (a sleeping reactor must still wake to
                // cancel an expired I/O operation). None = neither exists;
                // sleep until I/O completes or a wake arrives.
                //
                let timer_deadline = self
                    .delayed_futures
                    .peek()
                    .map(|delayed_future| delayed_future.activate_system_time);
                let io_timeout_deadline = self
                    .io_futures_with_timeout
                    .get_min_priority_node()
                    .map(|(_, _, deadline)| *deadline);

                let earliest_deadline = match (timer_deadline, io_timeout_deadline) {
                    (Some(timer), Some(io)) => Some(timer.min(io)),
                    (deadline, None) | (None, deadline) => deadline,
                };

                // Clock-domain note: deadlines are wall-clock (SystemTime,
                // pre-existing design — see tracker #62/#68 for the Instant
                // migration), but the kernel sleep measures the relative
                // duration monotonically. A wall-clock step during the sleep
                // therefore shifts firing by the step size at most once; the
                // old busy-spin re-read the wall clock continuously and would
                // mass-expire instead.
                let sleep_timeout = earliest_deadline.map(|deadline| {
                    deadline
                        .duration_since(SystemTime::now())
                        .unwrap_or(std::time::Duration::ZERO)
                });

                if matches!(sleep_timeout, Some(duration) if duration.is_zero()) {
                    // A deadline is already due; loop around and service it.
                    continue;
                }

                // Phase 3 — sleeping-flag handshake, then block.
                //
                // Sender side (enqueue_external_future_runtime): push; SeqCst
                // fence; if sleeping { wake() }. Sleeper side (here): store
                // sleeping; SeqCst fence; re-check the queue; block. The
                // paired fences guarantee at least one side observes the
                // other — without them this is the classic store-buffering
                // lost wakeup (sleeper misses the push AND sender misses the
                // flag). I/O completions need no flag: entering the kernel
                // wait observes already-arrived CQEs atomically.
                //
                self.sleeping
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);

                let shutdown_exit_ready = self
                    .is_in_shutdown
                    .load(std::sync::atomic::Ordering::Acquire)
                    && !self.io_manager.has_active_io()
                    && self.delayed_futures.is_empty();

                if !self.external_futures.is_empty() || shutdown_exit_ready {
                    // Work raced in, or the loop became exit-eligible
                    // (shutdown setters wake us, but may have checked the
                    // flag before we set it — this re-check closes that half
                    // of the race). Sleeping during a shutdown DRAIN is fine:
                    // pending I/O completions and timer deadlines both end
                    // the wait on their own.
                    self.sleeping
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    self.idle_streak = 0;
                    continue;
                }

                let _ = self.io_manager.wait_for_io(sleep_timeout);

                self.sleeping
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                self.idle_streak = 0;

                continue;
            }

            self.idle_streak = 0;

            // Process Future.
            //
            let mut future_runtime = option_future_runtime.unwrap();

            // SAFETY: We can use get_unchecked_mut because we won't move the future out.
            //
            unsafe {
                future_runtime
                    .as_mut()
                    .get_unchecked_mut()
                    .increment_execution_count()
            };

            self.schedule_reason = ScheduleReason::Pending;
            if future_runtime.is_first_yield() {
                self.yield_count = 0;
            } else {
                self.yield_count = 1;
            }

            // Do an initial poll.
            //
            if future_runtime
                .as_mut()
                .poll_future(&mut context)
                .is_pending()
            {
                // Reschedule the future.
                // Figure out the reason why we enqueue the future in the scheduler.
                //
                match self.schedule_reason {
                    ScheduleReason::None => {}
                    ScheduleReason::Pending => {}
                    ScheduleReason::Yield => {
                        // Move future to the end of the active ones.
                        //
                        unsafe {
                            future_runtime
                                .as_mut()
                                .get_unchecked_mut()
                                .set_first_yield(false)
                        };
                        self.active_futures.borrow_mut().push_back(future_runtime);
                    }
                    ScheduleReason::EventWait(event_future_ptr) => {
                        unsafe {
                            future_runtime
                                .as_mut()
                                .get_unchecked_mut()
                                .set_first_yield(true)
                        };

                        let event_future = unsafe { &mut *(event_future_ptr as *mut EventFuture) };
                        event_future.add_to_waiting(future_runtime);
                    }
                    ScheduleReason::WaitOne(wait_one_future_ptr) => {
                        unsafe {
                            future_runtime
                                .as_mut()
                                .get_unchecked_mut()
                                .set_first_yield(true)
                        };

                        let wait_one_future = unsafe { &*wait_one_future_ptr };
                        wait_one_future.set_waiting(future_runtime);
                    }
                    ScheduleReason::IOWait(io_wait_future_ptr) => {
                        unsafe {
                            future_runtime
                                .as_mut()
                                .get_unchecked_mut()
                                .set_first_yield(true)
                        };

                        let io_wait_future =
                            unsafe { &mut *{ io_wait_future_ptr as *mut IOWaitFuture } };

                        io_wait_future.set_future_runtime(future_runtime);

                        let io_result: Result<(), std::io::Error> =
                            self.io_manager.register_io_wait(io_wait_future_ptr);

                        // If IOWaitFuture has timeout, register it.
                        //
                        if let Some(timeout) = io_wait_future.timeout() {
                            self.register_timed_out_io_wait_future(
                                io_wait_future,
                                timeout.cancel_at_time(),
                            )
                        };

                        match io_result {
                            Ok(_) => {}
                            Err(io_error) => {
                                // Registration failed. Store the error and
                                // re-enqueue the task so it gets polled and
                                // sees the Err. push_front for I/O priority.
                                // Also remove any registered timeout: the task
                                // will be polled directly, so the entry is stale.
                                // Leaving it would cause cancel_io_wait to fire on
                                // a future that has already been re-enqueued, producing
                                // a UAF on Windows and an active_io imbalance on Linux.
                                if io_wait_future.timeout().is_some() {
                                    self.unregister_timed_out_io_wait_future(io_wait_future_ptr);
                                }
                                io_wait_future.set_io_result(Err(io_error));
                                if let Some(future_runtime) = io_wait_future.take_future_runtime() {
                                    self.active_futures.borrow_mut().push_front(future_runtime);
                                }
                            }
                        }
                    }
                    ScheduleReason::DelayedWait {
                        activate_system_time,
                    } => {
                        unsafe {
                            future_runtime
                                .as_mut()
                                .get_unchecked_mut()
                                .set_first_yield(true)
                        };

                        self.delayed_futures.push(ScheduledFutureRuntime::new(
                            activate_system_time,
                            future_runtime,
                        ));
                    }
                };
            } else {
                future_runtime.signal_completed();

                // Drop the future runtime as it has been fully executed.
                //
                drop(future_runtime);
            }
        }

        Reactor::reset_local_instance();
    }

    pub(crate) fn set_reactor_index(&mut self, index: usize) {
        self.index = index;
    }

    pub fn reactor_index(&self) -> usize {
        self.index
    }

    /// Returns the number of external futures waiting to be processed.
    /// This can be used as a load metric for reactor-aware scheduling.
    pub fn external_queue_len(&self) -> usize {
        self.external_futures.len()
    }

    pub fn create_event(&self) -> CooperativeEventFuture {
        CooperativeEventFuture::new(EventFuture::new(self))
    }

    pub(crate) fn create_wait_one(&self) -> (CooperativeWaitOneFuture, CooperativeWaitOneSignaler) {
        CooperativeWaitOneFuture::new(self)
    }

    pub fn create_fair_lock<T>(&self, data: T) -> CooperativeFairLock<T> {
        CooperativeFairLock::new(data)
    }

    pub(crate) fn io_manager<T: IOManager + 'static>(&mut self) -> Option<&mut T> {
        self.io_manager.as_mut().as_any().downcast_mut::<T>()
    }

    // Adds a Future to the active queue.
    //
    pub fn spawn<F: Future<Output = ()> + 'static>(
        &self,
        future: F,
    ) -> impl Future<Output = ()> + use<F> {
        let (mut completed_event_wait, completed_event_signaler) = self.create_wait_one();

        // Single allocation: Box contains both FutureRuntimeImpl and the future inline.
        //
        self.active_futures
            .borrow_mut()
            .push_back(Box::pin(FutureRuntimeImpl::new(
                future,
                CompletionSignaler::Cooperative(completed_event_signaler),
            )));

        async move {
            completed_event_wait.cooperative_wait().await;
        }
    }

    pub(crate) fn enqueue_future_runtimes(&self, incoming_futures: &mut VecDeque<FutureRuntime>) {
        self.active_futures.borrow_mut().append(incoming_futures);
    }

    pub(crate) fn enqueue_external_future_runtime(&self, future_runtime: FutureRuntime) {
        self.external_futures.push(future_runtime);

        // Sender half of the sleeping handshake (see run()): the SeqCst
        // fences pair so that either this load sees the sleeper's flag, or
        // the sleeper's post-flag re-check sees our push — never neither.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        if self.sleeping.load(std::sync::atomic::Ordering::Relaxed) {
            self.io_manager.wake();
        }
    }

    // Adds a Future with the result to the active queue.
    //
    pub fn spawn_with_result<T: 'static, F: Future<Output = T> + 'static>(
        &self,
        future: F,
    ) -> impl Future<Output = T> + use<T, F> {
        let result_consumer = Arc::new(ResultExternalFuture::new());
        let result_producer = Arc::clone(&result_consumer);

        let cooperative_future = async move {
            let result = future.await;

            // SAFETY: Producer writes before signaling completion.
            unsafe { result_producer.set(result) };
        };

        let completed_event = self.spawn(cooperative_future);

        async move {
            completed_event.await;

            // SAFETY: Completion event has fired — producer is done.
            unsafe { result_consumer.take() }
        }
    }

    /// Adds a future from external Reactor to the execution queue.
    ///
    fn enqueue_external<F: Future<Output = ()> + 'static + Send>(
        &self,
        future: F,
        completed_event_signaler: CompletionSignaler,
    ) {
        // Single allocation: Box contains both FutureRuntimeImpl and the future inline.
        //
        self.enqueue_external_future_runtime(Box::pin(FutureRuntimeImpl::new(
            future,
            completed_event_signaler,
        )));
    }

    // Spawns a Future from the external Reactor.
    //
    pub(super) fn spawn_external<F: Future<Output = ()> + 'static + Send>(
        &self,
        future: F,
    ) -> impl Future<Output = ()> + 'static + Send + use<F> {
        let (completed_event_wait, completed_event_signaler) = match Reactor::has_local_instance() {
            true => {
                let local_reactor = Reactor::local_instance();
                let (completed_event_wait, completed_event_signaler) =
                    local_reactor.create_wait_one();

                (
                    ExternalCompletionWaiter::Cooperative(completed_event_wait),
                    CompletionSignaler::Cooperative(completed_event_signaler),
                )
            }
            false => {
                let completed_event = Arc::new(CountdownEvent::new(1));

                (
                    ExternalCompletionWaiter::Preemptive(completed_event.clone()),
                    CompletionSignaler::Preemptive(completed_event),
                )
            }
        };

        self.enqueue_external(future, completed_event_signaler);

        async move {
            completed_event_wait.wait().await;
        }
    }

    // Spawns a Future with the result from the external Reactor.
    // This method assumes the Reactor has been created from the Coordinator and the local instance is set.
    //
    pub(crate) fn spawn_external_with_result<
        T: 'static + Send,
        F: Future<Output = T> + 'static + Send,
    >(
        &self,
        future: F,
    ) -> impl Future<Output = T> + 'static + Send + use<T, F> {
        let result_consumer = Arc::new(ResultExternalFuture::new());
        let result_producer = Arc::clone(&result_consumer);

        let local_completed_event = self.spawn_external(async move {
            let result = future.await;

            // SAFETY: Producer writes before signaling completion.
            unsafe { result_producer.set(result) };
        });

        async move {
            local_completed_event.await;

            // SAFETY: Completion event has fired — producer is done.
            unsafe { result_consumer.take() }
        }
    }

    /// Checks if given cooperative future is stil active.
    ///
    pub fn is_active_future<T: ReactorAssigned>(&self, cooperative_future: &T) -> bool {
        let cooperative_future_ptr = cooperative_future as *const T as *const c_void;
        self.active_futures.borrow().iter().any(|runtime| {
            // Get the raw pointer from the pinned boxed runtime.
            //
            let runtime_ptr =
                runtime.as_ref().get_ref() as *const dyn FutureRuntimeTrait as *const c_void;

            // And compare the pointers.
            //
            ptr::eq(runtime_ptr, cooperative_future_ptr)
        })
    }

    // If set to true, if there are no active futures or pending IO, reactor loop will exit.
    //
    pub fn set_is_in_shutdown_flag(&self, value: bool) {
        self.is_in_shutdown
            .store(value, std::sync::atomic::Ordering::Release);

        // Wake a reactor blocked in wait_for_io so it re-evaluates the exit
        // condition (same fence pairing as the external-enqueue path).
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        if self.sleeping.load(std::sync::atomic::Ordering::Relaxed) {
            self.io_manager.wake();
        }
    }

    pub fn has_local_instance() -> bool {
        let reactor_ptr = REACTOR_INSTANCE.with(|s| unsafe { *s.get() }) as *mut Reactor;

        !reactor_ptr.is_null()
    }

    /// # Preconditions
    /// Must be called only on threads where `set_local_instance` was executed and
    /// before `reset_local_instance`. The TLS raw pointer must remain valid.
    #[inline]
    pub fn local_instance<'a>() -> &'a mut Self {
        let reactor_ptr = REACTOR_INSTANCE.with(|s| unsafe { *s.get() }) as *mut Reactor;

        debug_assert!(!reactor_ptr.is_null(), "Reactor TLS not initialized");

        unsafe { &mut *reactor_ptr }
    }

    pub(crate) fn register_timed_out_io_wait_future(
        &mut self,
        io_wait_future: *const IOWaitFuture,
        timeout: SystemTime,
    ) {
        self.io_futures_with_timeout
            .insert(io_wait_future, io_wait_future, timeout);
    }

    pub(crate) fn unregister_timed_out_io_wait_future(
        &mut self,
        io_wait_future: *const IOWaitFuture,
    ) {
        self.io_futures_with_timeout.remove(&io_wait_future);
    }

    pub(super) fn set_local_instance(&self) {
        REACTOR_INSTANCE.with(|s| {
            unsafe {
                *s.get() = self;
            };
        });
    }

    pub(super) fn reset_local_instance() {
        REACTOR_INSTANCE.with(|s| {
            unsafe {
                *s.get() = ptr::null_mut();
            };
        });
    }
}

impl Default for Reactor {
    fn default() -> Self {
        Self::new()
    }
}

pub type CooperativeReactor = RcPointer<Reactor>;

// Reactor Thread Instance.
//
thread_local! {
    static REACTOR_INSTANCE: UnsafeCell<*const Reactor> = const {UnsafeCell::new(ptr::null_mut())};
}

// Wrapper for the Reactor so it could be used from the other cooperative threads.
//
pub struct ExternalReactor<'a> {
    reactor: &'a mut Reactor,
}

impl<'a> ExternalReactor<'a> {
    pub fn new(reactor: &'a mut Reactor) -> Self {
        ExternalReactor { reactor }
    }

    // Spawns a Future from the external Reactor.
    //
    pub fn spawn_external<F: Future<Output = ()> + 'static + Send>(
        &self,
        future: F,
    ) -> impl Future<Output = ()> + 'static + Send + use<F> {
        self.reactor.spawn_external(future)
    }

    // Spawns a Future with the result from the external Reactor.
    //
    pub fn spawn_external_with_result<T: 'static + Send, F: Future<Output = T> + 'static + Send>(
        &self,
        future: F,
    ) -> impl Future<Output = T> + 'static + Send + use<T, F> {
        self.reactor.spawn_external_with_result(future)
    }
}

// Waker implementation.
//
fn create_waker(reactor: &Reactor) -> Waker {
    unsafe { Waker::from_raw(create_raw_waker(reactor)) }
}

unsafe fn create_raw_waker(reactor: &Reactor) -> RawWaker {
    RawWaker::new(
        ptr::from_ref(reactor) as *const (),
        &RawWakerVTable::new(clone_waker, wake, wake_by_ref, drop_waker),
    )
}

unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
    unsafe {
        let reactor = (ptr as *const Reactor).as_ref().unwrap();
        create_raw_waker(reactor)
    }
}

unsafe fn wake(_: *const ()) {
    // Noop.
}

unsafe fn wake_by_ref(_: *const ()) {
    // Noop.
}

unsafe fn drop_waker(_: *const ()) {
    // Noop.
}

// Implements YieldFuture.
// Polling this future allows the reactor to stop executing the future, put it at the end of the active queue and execute next future in the queue.
//
struct YieldFuture {}

impl YieldFuture {
    fn new() -> Self {
        YieldFuture {}
    }
}

impl Future for YieldFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        // Get the reactor from the context.
        //
        let reactor = Reactor::local_instance();

        // Update the reactor with rescheduling reason.
        //
        reactor.schedule_reason = ScheduleReason::Yield;

        let mut should_continue = false;
        if reactor.yield_count > 0 {
            reactor.yield_count -= 1;
            should_continue = true;
        }

        if should_continue {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[inline]
pub async fn cooperative_yield() {
    YieldFuture::new().await;
}

/// Helpers for unit tests that drive an `IOManager` directly, outside a
/// running reactor.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;

    use super::{CompletionSignaler, FutureRuntime, FutureRuntimeImpl};

    /// Sets the wrapped flag when dropped, so tests can observe that a parked
    /// task box was actually freed.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// Builds a minimal `FutureRuntime` whose destruction is observable via
    /// `dropped_flag`. The future itself never needs to be polled.
    pub(crate) fn test_future_runtime(dropped_flag: Arc<AtomicBool>) -> FutureRuntime {
        let guard = DropFlag(dropped_flag);

        Box::pin(FutureRuntimeImpl::new(
            async move {
                let _guard = guard;
            },
            CompletionSignaler::Preemptive(Arc::new(CountdownEvent::new(1))),
        ))
    }
}
