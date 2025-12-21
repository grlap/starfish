use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll};

use crate::rc_pointer::RcPointer;
use crate::reactor::{FutureRuntime, Reactor, ReactorAssigned, ScheduleReason};

// Implements EventFuture.
//
pub type CooperativeEventFuture = RcPointer<EventFuture>;

pub struct EventFuture {
    pub(crate) reactor_ptr: *mut Reactor,
    waiting_futures: RefCell<VecDeque<FutureRuntime>>,
    is_signaled: Cell<bool>,
}

impl EventFuture {
    pub fn new(reactor: &Reactor) -> Self {
        let reactor_ptr = reactor as *const _ as *mut _;

        EventFuture {
            reactor_ptr,
            waiting_futures: RefCell::new(VecDeque::default()),
            is_signaled: Cell::new(false),
        }
    }

    pub fn new_no_reactor() -> Self {
        EventFuture {
            reactor_ptr: ptr::null_mut(),
            waiting_futures: RefCell::new(VecDeque::default()),
            is_signaled: Cell::new(false),
        }
    }

    pub(crate) fn add_to_waiting(&self, waiting_future: FutureRuntime) {
        self.waiting_futures.borrow_mut().push_back(waiting_future);
    }

    pub fn signal(&self) {
        self.is_signaled.set(true);

        if let Some(reactor) = self.assigned_reactor()
            && !self.waiting_futures.borrow().is_empty()
        {
            // Move waiters to the active futures.
            // Enqueue waiting futures in the reactor.
            //
            reactor.enqueue_future_runtimes(&mut self.waiting_futures.borrow_mut());
        }
    }
}

impl Future for EventFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        // Get the reactor from the context.
        //
        let reactor = Reactor::local_instance();

        if self.is_signaled.get() {
            Poll::Ready(())
        } else {
            let event_future_ptr: *const EventFuture = ptr::addr_of!(*self);

            // Update the reactor with rescheduling reason.
            //
            reactor.set_schedule_reason(ScheduleReason::EventWait(event_future_ptr));

            Poll::Pending
        }
    }
}

impl ReactorAssigned for EventFuture {
    fn assigned_reactor(&self) -> Option<&Reactor> {
        unsafe {
            match self.reactor_ptr {
                reactor_ptr if reactor_ptr.is_null() => None,

                reactor_ptr => Some(&*reactor_ptr),
            }
        }
    }
}

// Implement Drop trait.
//
#[cfg(debug_assertions)]
impl Drop for EventFuture {
    fn drop(&mut self) {
        // Verify the EventFuture is no longer part of cooperative scheduler.
        //
        match self.assigned_reactor() {
            Some(reactor) => {
                let is_active = reactor.is_active_future(self);

                if is_active {
                    panic!(
                        "Unable to drop EventFuture. The future is still active in the reactor."
                    );
                }
            }
            None => {
                // EventFuture was created on preemptive thread without Reactor.
                //
            }
        }
    }
}

impl CooperativeEventFuture {
    pub async fn cooperative_wait(&self) {
        let mut event_clone = self.clone();
        let pinned_future = unsafe { Pin::new_unchecked(event_clone.get_raw_mut()) };

        if !pinned_future.is_signaled.get() {
            pinned_future.await;
        }
    }
}
