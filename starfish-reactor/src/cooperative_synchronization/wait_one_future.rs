use std::cell::UnsafeCell;
use std::future::Future;
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use crate::rc_pointer::ArcPointer;
use crate::reactor::{FutureRuntime, Reactor, ReactorAssigned, ScheduleReason};

// Implements WaitOneFuture.
//

struct WaitOneSharedState {
    pub(crate) reactor_ptr: *mut Reactor,
    waiting_future: UnsafeCell<Option<FutureRuntime>>,
    is_signaled: AtomicBool,
}

pub(crate) struct CooperativeWaitOneFuture {
    state: ArcPointer<WaitOneSharedState>,
}

unsafe impl Send for CooperativeWaitOneFuture {}
unsafe impl Sync for CooperativeWaitOneFuture {}

pub(crate) struct CooperativeWaitOneSignaler {
    state: ArcPointer<WaitOneSharedState>,
}

unsafe impl Send for CooperativeWaitOneSignaler {}
unsafe impl Sync for CooperativeWaitOneSignaler {}

impl WaitOneSharedState {
    pub(crate) fn new(reactor: &Reactor) -> Self {
        let reactor_ptr = reactor as *const _ as *mut _;

        WaitOneSharedState {
            reactor_ptr,
            waiting_future: UnsafeCell::new(None),
            is_signaled: AtomicBool::new(false),
        }
    }

    pub(super) fn new_no_reactor() -> Self {
        WaitOneSharedState {
            reactor_ptr: ptr::null_mut(),
            waiting_future: UnsafeCell::new(None),
            is_signaled: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_waiting(&mut self, waiting_future: FutureRuntime) {
        // Store the future BEFORE the swap so signal() can see it.
        //
        unsafe {
            *self.waiting_future.get() = Some(waiting_future);
        }

        // Now swap. If signal() already ran (was_signaled=true), take the future back and enqueue.
        // If signal() hasn't run yet, it will take the future when it does.
        //
        let was_signaled = self.is_signaled.swap(true, Ordering::AcqRel);

        if was_signaled {
            // signal() already ran - take the future we just stored and enqueue directly.
            //
            let future_runtime = unsafe { (*self.waiting_future.get()).take().unwrap() };

            match self.assigned_reactor() {
                Some(reactor) => {
                    reactor.enqueue_external_feature_runtime(future_runtime);
                }
                None => todo!(),
            }
        }
    }

    pub(super) fn signal(&self) {
        // Swap to set signaled. If set_waiting() already ran (was_signaled=true),
        // the future is stored and we take it. If not, set_waiting() will enqueue directly.
        //
        let was_signaled = self.is_signaled.swap(true, Ordering::AcqRel);

        if was_signaled {
            // set_waiting() already ran and stored the future - take it.
            //
            let future_runtime = unsafe { (*self.waiting_future.get()).take().unwrap() };

            match self.assigned_reactor() {
                Some(reactor) => {
                    reactor.enqueue_external_feature_runtime(future_runtime);
                }
                None => todo!(),
            }
        }
        // If was_signaled was false, set_waiting() hasn't run yet.
        // When it does run, it will see is_signaled=true and enqueue directly.
    }
}

impl ReactorAssigned for WaitOneSharedState {
    fn assigned_reactor(&self) -> Option<&Reactor> {
        unsafe {
            match self.reactor_ptr {
                reactor_ptr if reactor_ptr.is_null() => None,

                reactor_ptr => Some(&*reactor_ptr),
            }
        }
    }
}

impl ReactorAssigned for CooperativeWaitOneFuture {
    fn assigned_reactor(&self) -> Option<&Reactor> {
        self.state.assigned_reactor()
    }
}

impl ReactorAssigned for CooperativeWaitOneSignaler {
    fn assigned_reactor(&self) -> Option<&Reactor> {
        self.state.assigned_reactor()
    }
}

impl CooperativeWaitOneSignaler {
    pub(crate) fn signal(&self) {
        self.state.signal();
    }
}

impl Future for CooperativeWaitOneFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        // Get the reactor from the context.
        //
        let reactor = Reactor::local_instance();

        if self.state.is_signaled.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            let wait_one_future_ptr: *const CooperativeWaitOneFuture = ptr::addr_of!(*self);

            // Update the reactor with rescheduling reason.
            //
            reactor.set_schedule_reason(ScheduleReason::WaitOne(wait_one_future_ptr));

            Poll::Pending
        }
    }
}

impl CooperativeWaitOneFuture {
    pub(crate) fn new(reactor: &Reactor) -> (Self, CooperativeWaitOneSignaler) {
        let wait_one_shared_state = ArcPointer::new(WaitOneSharedState::new(reactor));

        (
            CooperativeWaitOneFuture {
                state: wait_one_shared_state.clone(),
            },
            CooperativeWaitOneSignaler {
                state: wait_one_shared_state,
            },
        )
    }

    pub(crate) fn new_no_reactor() -> (Self, CooperativeWaitOneSignaler) {
        let wait_one_shared_state = ArcPointer::new(WaitOneSharedState::new_no_reactor());

        (
            CooperativeWaitOneFuture {
                state: wait_one_shared_state.clone(),
            },
            CooperativeWaitOneSignaler {
                state: wait_one_shared_state,
            },
        )
    }

    pub(crate) fn set_waiting(&mut self, waiting_future: FutureRuntime) {
        self.state.set_waiting(waiting_future);
    }

    pub(crate) async fn cooperative_wait(&mut self) {
        if !self.state.is_signaled.load(Ordering::Acquire) {
            self.await;
        }
    }
}
