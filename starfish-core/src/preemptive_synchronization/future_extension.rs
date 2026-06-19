//! Synchronous future evaluation for testing and initialization.
//!
//! Provides `FutureExtension::unwrap_result()`, which polls a future
//! exactly once with a no-op waker and panics if it returns `Pending`.
//! Useful for evaluating futures known to be immediately ready.

use std::future::Future;
use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

pub trait FutureExtension: Future {
    /// Gets a result from the future.
    /// This function will panic if the future returns `Poll::Pending`.
    ///
    fn unwrap_result(self) -> Self::Output;
}

impl<F: Future> FutureExtension for F {
    fn unwrap_result(self) -> Self::Output {
        // Create a dummy waker.
        //
        fn dummy_waker() -> Waker {
            static VTABLE: RawWakerVTable = RawWakerVTable::new(
                |_| RawWaker::new(ptr::null(), &VTABLE),
                |_| {},
                |_| {},
                |_| {},
            );
            // SAFETY: The VTABLE is a valid, 'static RawWakerVTable whose clone/wake/drop
            // functions are all no-ops, so the resulting Waker will never dereference the
            // null data pointer. This satisfies Waker::from_raw's contract.
            unsafe { Waker::from_raw(RawWaker::new(ptr::null(), &VTABLE)) }
        }

        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);

        let mut future = self;

        // Pin the future on the stack
        // SAFETY: `future` is a local variable that is never moved after this point,
        // so the Pin contract (the referent will not be moved) is upheld.
        let mut pinned = unsafe { Pin::new_unchecked(&mut future) };

        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(val) => val,
            Poll::Pending => panic!("expected completed future"),
        }
    }
}
