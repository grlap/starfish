use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::SystemTime;
use std::{io, ptr};

use binary_heap_plus::{BinaryHeap, MinComparator};
use crossbeam::queue::SegQueue;

use crate::cooperative_io::DefaultIOManager;
use crate::cooperative_io::io_manager::{IOManager, IOManagerCreateOptions, IOManagerHolder};
use crate::cooperative_io::io_wait_future::IOWaitFuture;
use crate::cooperative_synchronization::event_future::{CooperativeEventFuture, EventFuture};
use crate::cooperative_synchronization::lock::CooperativeFairLock;
use crate::cooperative_synchronization::wait_one_future::CooperativeWaitOneFuture;
use crate::cooperative_synchronization::wait_one_future::CooperativeWaitOneSignaler;
use crate::rc_pointer::RcPointer;
use starfish_core::data_structures::Treap;

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
// Uses CooperativeWaitOneSignaler for signaling.
//
pub struct FutureRuntimeImpl<F: Future<Output = ()>> {
    future: F,
    is_first_yield: bool,
    execution_count: u32,
    completed_signaler: CooperativeWaitOneSignaler,
}

impl<F: Future<Output = ()>> FutureRuntimeImpl<F> {
    fn new(future: F, completed_signaler: CooperativeWaitOneSignaler) -> Self {
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
}

impl Reactor {
    pub fn new() -> Self {
        Self::try_new().unwrap()
    }

    pub fn try_new() -> io::Result<Self> {
        Ok(Self::with_io_manager(IOManagerHolder {
            io_manager: Box::new(DefaultIOManager::try_new()?),
        }))
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
        }
    }

    /// Sets the scheduling reason for a given future.
    ///
    #[inline]
    pub(super) fn set_schedule_reason(&mut self, reason: ScheduleReason) {
        self.schedule_reason = reason;
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
            // Cancel timeouted IOWaitFutures.
            //
            while let Some((key, _, timeout)) = self.io_futures_with_timeout.get_min_priority_node()
            {
                if *timeout >= SystemTime::now() {
                    // IOWaiteFuture expired. Cancel outstanding IO, ignore any errors.
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

            // Handle external futures first.
            //
            let mut option_future_runtime = self.external_futures.pop();

            if option_future_runtime.is_none() && self.io_manager.has_active_io() {
                let completed_io = self.io_manager.completed_io();

                if let Some((future_runtime, io_wait_future_ptr)) = completed_io {
                    option_future_runtime = Some(future_runtime);

                    let io_wait_future = unsafe { &*io_wait_future_ptr };

                    if io_wait_future.timeout().is_some() {
                        // If completed IOWaitFuture has timeout, remove the future from io_futures_with_timeout.
                        //
                        let const_ptr: *const IOWaitFuture = io_wait_future_ptr as *const _;
                        self.io_futures_with_timeout.remove(&const_ptr);
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
                continue;
            }

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

                        let wait_one_future =
                            unsafe { &mut *(wait_one_future_ptr as *mut CooperativeWaitOneFuture) };
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
                            self.register_timeouted_io_wait_future(
                                io_wait_future,
                                timeout.cancel_at_time(),
                            )
                        };

                        match io_result {
                            Ok(_) => {}
                            Err(io_error) => {
                                // We failed to register pending IO.
                                //
                                io_wait_future.set_io_result(Err(io_error));
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
        let (mut completed_event_wait, copleted_event_signaler) = self.create_wait_one();

        // Single allocation: Box contains both FutureRuntimeImpl and the future inline.
        //
        self.active_futures
            .borrow_mut()
            .push_back(Box::pin(FutureRuntimeImpl::new(
                future,
                copleted_event_signaler,
            )));

        async move {
            completed_event_wait.cooperative_wait().await;
        }
    }

    pub(crate) fn enqueue_future_runtimes(&self, incoming_futures: &mut VecDeque<FutureRuntime>) {
        self.active_futures.borrow_mut().append(incoming_futures);
    }

    pub(crate) fn enqueue_external_feature_runtime(&self, future_runtime: FutureRuntime) {
        self.external_futures.push(future_runtime);
    }

    // Adds a Future with the result to the active queue.
    //
    pub fn spawn_with_result<T: 'static, F: Future<Output = T> + 'static>(
        &self,
        future: F,
    ) -> impl Future<Output = T> + use<T, F> {
        let pinned_result_ptr_addr = Box::into_raw(Box::new(None::<T>)) as usize;

        let cooperative_future = async move {
            let result = future.await;

            unsafe {
                let pinned_result_ptr = pinned_result_ptr_addr as *mut Option<T>;
                *pinned_result_ptr = Some(result);
            }
        };

        let completed_event = self.spawn(cooperative_future);

        async move {
            completed_event.await;

            unsafe {
                let pinned_result_ptr = pinned_result_ptr_addr as *mut Option<T>;
                // Convert raw pointer back to Box (reclaims ownership).
                //
                let boxed = Box::from_raw(pinned_result_ptr);

                // Dereference the Box to move out its value.
                // The Box and its allocated memory are automatically freed.
                //
                (*boxed).unwrap()
            }
        }
    }

    /// Adds a future from external Reactor to the execution queue.
    ///
    fn enqueue_external<F: Future<Output = ()> + 'static + Send>(
        &self,
        future: F,
        completed_event_signaler: CooperativeWaitOneSignaler,
    ) {
        // Single allocation: Box contains both FutureRuntimeImpl and the future inline.
        //
        self.external_futures.push(Box::pin(FutureRuntimeImpl::new(
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
        let (mut completed_event_wait, completed_event_signaler) =
            match Reactor::has_local_instance() {
                true => {
                    let local_reactor = Reactor::local_instance();

                    local_reactor.create_wait_one()
                }
                false => CooperativeWaitOneFuture::new_no_reactor(),
            };

        self.enqueue_external(future, completed_event_signaler);

        async move {
            completed_event_wait.cooperative_wait().await;
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
        let pinned_result_ptr_addr = Box::into_raw(Box::new(None::<T>)) as usize;

        let local_completed_event = self.spawn_external(async move {
            let result = future.await;

            unsafe {
                let pinned_result_ptr = pinned_result_ptr_addr as *mut Option<T>;
                *pinned_result_ptr = Some(result);
            }
        });

        async move {
            local_completed_event.await;

            unsafe {
                let pinned_result_ptr = pinned_result_ptr_addr as *mut Option<T>;

                // Convert raw pointer back to Box (reclaims ownership).
                //
                let boxed = Box::from_raw(pinned_result_ptr);

                // Dereference the Box to move out its value.
                // The Box and its allocated memory are automatically freed.
                //
                (*boxed).unwrap()
            }
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
    }

    pub fn has_local_instance() -> bool {
        let reactor_ptr = REACTOR_INSTANCE.with(|s| unsafe { *s.get() }) as *mut Reactor;

        !reactor_ptr.is_null()
    }

    pub fn local_instance<'a>() -> &'a mut Self {
        let reactor_ptr = REACTOR_INSTANCE.with(|s| unsafe { *s.get() }) as *mut Reactor;

        unsafe { &mut *reactor_ptr }
    }

    pub(crate) fn register_timeouted_io_wait_future(
        &mut self,
        io_wait_future: *const IOWaitFuture,
        timeout: SystemTime,
    ) {
        self.io_futures_with_timeout
            .insert(io_wait_future, io_wait_future, timeout);
    }

    pub(crate) fn unregister_timeouted_io_wait_future(
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
    pub fn spawn_external_with_result<
        T: Default + 'static + Send,
        F: Future<Output = T> + 'static + Send,
    >(
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

// Implements YieldFutre.
// Pooling this future allows the reactor to stop executing the future, put it at the end of the active queue and execute next future in the queue.
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
