//! Cooperative Locks for Async Reactors
//!
//! This module implements cooperative locks designed for use with the starfish reactor.
//! These locks integrate with the cooperative scheduler to yield execution while waiting.
//!
//! # Ownership Transfer
//!
//! On release, ownership transfers directly to the next waiter (if any).
//! The `has_owner` flag stays `true` during transfer - no re-acquisition needed.
//!
//! ```text
//!   ┌─────────┐                              ┌─────────┐
//!   │ Owner A │──release()──► signal(B) ───► │ Owner B │
//!   └─────────┘               (ownership     └─────────┘
//!                              transfer)
//! ```
//!
//! # Acquisition Flow
//!
//! ```text
//!   Task wants lock
//!         │
//!         ▼
//!   ┌─────────────┐     yes    ┌──────────────────┐
//!   │ try_acquire ├───────────►│ Lock acquired!   │
//!   └──────┬──────┘            │ Return immediately│
//!          │ no                └──────────────────┘
//!          ▼
//!   ┌─────────────────────────┐
//!   │ Create waiter (signaler)│
//!   │ Push to waiting queue   │
//!   └───────────┬─────────────┘
//!               │
//!               ▼
//!   ┌─────────────────────────┐     yes    ┌─────────────────────┐
//!   │ try_acquire again       ├───────────►│ Won race! But we    │
//!   │ (race condition check)  │            │ have signaler queued│
//!   └───────────┬─────────────┘            │ Call release() so   │
//!               │ no                       │ we receive ownership│
//!               │                          │ via signal instead  │
//!               │                          └─────────┬───────────┘
//!               │                                    │
//!               ▼                                    ▼
//!   ┌─────────────────────────┐         ┌─────────────────────────┐
//!   │ cooperative_wait()      │◄────────┤ Wait for our signal     │
//!   │ Yield to reactor        │         └─────────────────────────┘
//!   └───────────┬─────────────┘
//!               │
//!               ▼
//!   ┌─────────────────────────┐
//!   │ Woken up by signal      │
//!   │ We now OWN the lock     │
//!   │ (ownership transferred) │
//!   └─────────────────────────┘
//! ```
//!
//! # Release Flow (FairLock)
//!
//! ```text
//!   Owner calls release (via Drop)
//!         │
//!         ▼
//!   ┌─────────────────┐
//!   │ Pop from queue  │
//!   └────────┬────────┘
//!            │
//!     ┌──────┴──────┐
//!     │             │
//!     ▼             ▼
//!   Waiter        No waiter
//!   exists        in queue
//!     │             │
//!     ▼             ▼
//!   ┌─────────┐   ┌──────────────────┐
//!   │signal() │   │ has_owner = false│
//!   │(transfer│   │ Lock is free     │
//!   │ownership│   └──────────────────┘
//!   └─────────┘
//!       │
//!       ▼
//!   ┌─────────────────────────┐
//!   │ Waiter wakes up and     │
//!   │ already owns the lock!  │
//!   │ (has_owner stays TRUE)  │
//!   └─────────────────────────┘
//! ```
//!
//! # Lock Types
//!
//! ```text
//!   FairLock (FIFO order):
//!   ┌─────────────────────────────────────┐
//!   │ Queue: [B, C, D] ──► B gets lock    │
//!   │        (first in, first out)        │
//!   └─────────────────────────────────────┘
//!
//!   UnfairLock (Random selection):
//!   ┌─────────────────────────────────────┐
//!   │ Queue: [B, C, D] ──► random(B,C,D)  │
//!   │        (any waiter may be chosen)   │
//!   └─────────────────────────────────────┘
//!
//!   ReactorAwareLock (Least loaded reactor, round-based):
//!   ┌─────────────────────────────────────────────────────┐
//!   │ Waiters: [B@R0, C@R1, D@R2]                         │
//!   │ Reactor loads: R0=5, R1=1, R2=8                     │
//!   │                     ──► C@R1 gets lock              │
//!   │ (picks waiter on reactor with shortest queue)       │
//!   │                                                     │
//!   │ Round-based scanning prevents starvation:           │
//!   │   counter=0 → new round, counter=waiters.len()      │
//!   │   Each release: scan first `counter` waiters        │
//!   │   Decrement counter after each release              │
//!   │   Window shrinks → old waiters guaranteed progress  │
//!   └─────────────────────────────────────────────────────┘
//! ```
//!
//! # Why No Loop in acquire_internal?
//!
//! Traditional locks need a loop because waiters must re-compete after waking.
//! Here, the signal **transfers ownership**, so when `cooperative_wait()` returns,
//! the task already owns the lock - no re-acquisition needed.
//!
//! # Cross-Reactor Safety
//!
//! These locks are safe across reactor boundaries:
//! - `external_waiting_futures` uses `SegQueue` (lock-free, thread-safe)
//! - `has_owner` uses atomic operations
//! - Waiters from any reactor can be queued and signaled

use std::cell::RefCell;
use std::cell::UnsafeCell;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crossbeam::queue::SegQueue;
use rand::Rng;

use crate::cooperative_synchronization::wait_one_future::CooperativeWaitOneSignaler;
use crate::rc_pointer::ArcPointer;
use crate::reactor::Reactor;
use crate::reactor::ReactorAssigned;

struct LockSync {
    external_waiting_futures: SegQueue<CooperativeWaitOneSignaler>,
    has_owner: AtomicBool,
}

// LockSync.
//
impl LockSync {
    pub(self) fn new() -> Self {
        LockSync {
            external_waiting_futures: SegQueue::new(),
            has_owner: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub(self) fn try_acquire(&self) -> bool {
        let has_owner = self.has_owner.load(Ordering::Relaxed);
        match has_owner {
            true => false,
            false => {
                // Try to acquire the lock.
                //
                self.has_owner
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            }
        }
    }

    pub(self) fn release(&self) {
        self.has_owner.store(false, Ordering::Release);
    }
}

trait Lock<T: ?Sized> {
    fn release(&self);
}

trait LockExt<T: ?Sized>: Lock<T> {
    async fn acquire_internal(&self, lock_sync: &LockSync) {
        // Try to acquire the lock directly.
        //
        if lock_sync.try_acquire() {
            return;
        }

        // Failed to acquire the Lock. Enqueue ourselves into the waiting queue.
        //
        let (mut wait_one_waiter, wait_one_signaler) = Reactor::local_instance().create_wait_one();
        lock_sync.external_waiting_futures.push(wait_one_signaler);

        // Handle race condition: the lock might have been released between our
        // try_acquire and pushing to the queue. Try again.
        //
        if lock_sync.try_acquire() {
            // We won the race and acquired the lock directly.
            // But we also have a signaler in the queue that might get signaled.
            // Release the lock to maintain the invariant (signaler in queue = waiting).
            // We'll receive ownership back via signal.
            //
            self.release();
        }

        // Wait for signal. When we wake up, we own the lock.
        //
        wait_one_waiter.cooperative_wait().await;
    }
}

// Blanket implementation: ALL Lock<T> types get these methods.
//
impl<T: ?Sized, L: Lock<T>> LockExt<T> for L {}

// Implements Synchronization FairLock.
//
pub struct CooperativeFairLock<T: ?Sized>(ArcPointer<FairLock<T>>);

impl<T> CooperativeFairLock<T> {
    pub fn new(data: T) -> Self {
        Self(ArcPointer::new(FairLock::new(data)))
    }
}

impl<T: ?Sized> Clone for CooperativeFairLock<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: ?Sized> Deref for CooperativeFairLock<T> {
    type Target = FairLock<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct FairLock<T: ?Sized> {
    lock_sync: LockSync,
    data: UnsafeCell<T>,
}

// FairLock.
//
impl<T> FairLock<T> {
    fn new(data: T) -> Self {
        FairLock {
            lock_sync: LockSync::new(),
            data: UnsafeCell::new(data),
        }
    }

    #[must_use]
    pub fn try_acquire(&self) -> Option<LockResult<'_, T>> {
        match self.lock_sync.try_acquire() {
            true => Some(LockResult {
                lock: self,
                data: unsafe { &mut *self.data.get() },
            }),
            false => None,
        }
    }

    #[must_use]
    pub async fn acquire(&self) -> LockResult<'_, T> {
        self.acquire_internal(&self.lock_sync).await;

        LockResult {
            lock: self,
            data: unsafe { &mut *self.data.get() },
        }
    }
}

impl<T: ?Sized> Lock<T> for FairLock<T> {
    fn release(&self) {
        // Extract the signaler to wake (if any), then signal AFTER to avoid
        // re-entrancy issues. signal() can synchronously wake a task on the
        // same thread, which may then call release() again.
        //
        let signaler_to_wake = match self.lock_sync.external_waiting_futures.pop() {
            Some(wait_one) => Some(wait_one),
            None => {
                // No waiters, set lock as not acquired.
                //
                self.lock_sync.release();
                None
            }
        };

        // Signal outside any potential critical section.
        //
        if let Some(signaler) = signaler_to_wake {
            signaler.signal();
        }
    }
}

unsafe impl<T: ?Sized> Send for FairLock<T> {}
unsafe impl<T: ?Sized> Sync for FairLock<T> {}

// Implements Synchronization Unfair Lock.
//
pub struct CooperativeUnfairLock<T: ?Sized>(ArcPointer<UnfairLock<T>>);

impl<T> CooperativeUnfairLock<T> {
    pub fn new(data: T) -> Self {
        Self(ArcPointer::new(UnfairLock::new(data)))
    }
}

impl<T: ?Sized> Clone for CooperativeUnfairLock<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: ?Sized> Deref for CooperativeUnfairLock<T> {
    type Target = UnfairLock<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct UnfairLock<T: ?Sized> {
    lock_sync: LockSync,
    external_waiting_futures: RefCell<Vec<CooperativeWaitOneSignaler>>,
    data: UnsafeCell<T>,
}

// UnfairLock.
//
impl<T> UnfairLock<T> {
    fn new(data: T) -> Self {
        UnfairLock {
            lock_sync: LockSync::new(),
            external_waiting_futures: RefCell::new(vec![]),
            data: UnsafeCell::new(data),
        }
    }

    #[must_use]
    pub fn try_acquire(&self) -> Option<LockResult<'_, T>> {
        match self.lock_sync.try_acquire() {
            true => Some(LockResult {
                lock: self,
                data: unsafe { &mut *self.data.get() },
            }),
            false => None,
        }
    }

    #[must_use]
    pub async fn acquire(&self) -> LockResult<'_, T> {
        self.acquire_internal(&self.lock_sync).await;

        LockResult {
            lock: self,
            data: unsafe { &mut *self.data.get() },
        }
    }
}

impl<T: Sized> Lock<T> for UnfairLock<T> {
    fn release(&self) {
        // Extract the signaler to wake (if any) while holding the borrow,
        // then signal AFTER releasing the borrow to avoid re-entrancy panic.
        // signal() can synchronously wake a task on the same thread, which
        // may then call release() again - we must not hold the RefCell borrow.
        //
        let signaler_to_wake = {
            let mut waiters = self.external_waiting_futures.borrow_mut();

            // Move all waiters from lock-free queue into internal vector.
            //
            while let Some(wait_one) = self.lock_sync.external_waiting_futures.pop() {
                waiters.push(wait_one);
            }

            if waiters.is_empty() {
                // No waiters, set lock as not acquired.
                //
                self.lock_sync.release();
                None
            } else {
                let mut rng = rand::rng();
                let random_index = rng.random_range(0..waiters.len());

                // Remove random element - signal after dropping borrow.
                //
                Some(waiters.swap_remove(random_index))
            }
        };

        // Signal outside the borrow to prevent re-entrancy panic.
        //
        if let Some(signaler) = signaler_to_wake {
            signaler.signal();
        }
    }
}

unsafe impl<T: ?Sized> Send for UnfairLock<T> {}
unsafe impl<T: ?Sized> Sync for UnfairLock<T> {}

// Implements Synchronization Reactor-Aware Lock.
// Selects the waiter on the least loaded reactor.
//
pub struct CooperativeReactorAwareLock<T: ?Sized>(ArcPointer<ReactorAwareLock<T>>);

impl<T> CooperativeReactorAwareLock<T> {
    pub fn new(data: T) -> Self {
        Self(ArcPointer::new(ReactorAwareLock::new(data)))
    }
}

impl<T: ?Sized> Clone for CooperativeReactorAwareLock<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: ?Sized> Deref for CooperativeReactorAwareLock<T> {
    type Target = ReactorAwareLock<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct ReactorAwareLock<T: ?Sized> {
    lock_sync: LockSync,
    external_waiting_futures: RefCell<Vec<CooperativeWaitOneSignaler>>,
    /// Round-based scan counter. When 0, starts new round with current waiter count.
    /// Decrements each release, limiting scan window to prevent starvation.
    scan_counter: RefCell<usize>,
    data: UnsafeCell<T>,
}

// ReactorAwareLock.
//
impl<T> ReactorAwareLock<T> {
    fn new(data: T) -> Self {
        ReactorAwareLock {
            lock_sync: LockSync::new(),
            external_waiting_futures: RefCell::new(vec![]),
            scan_counter: RefCell::new(0),
            data: UnsafeCell::new(data),
        }
    }

    #[must_use]
    pub fn try_acquire(&self) -> Option<LockResult<'_, T>> {
        match self.lock_sync.try_acquire() {
            true => Some(LockResult {
                lock: self,
                data: unsafe { &mut *self.data.get() },
            }),
            false => None,
        }
    }

    #[must_use]
    pub async fn acquire(&self) -> LockResult<'_, T> {
        self.acquire_internal(&self.lock_sync).await;

        LockResult {
            lock: self,
            data: unsafe { &mut *self.data.get() },
        }
    }
}

impl<T: Sized> Lock<T> for ReactorAwareLock<T> {
    fn release(&self) {
        // Extract the signaler to wake (if any) while holding the borrow,
        // then signal AFTER releasing the borrow to avoid re-entrancy panic.
        // signal() can synchronously wake a task on the same thread, which
        // may then call release() again - we must not hold the RefCell borrow.
        //
        let signaler_to_wake = {
            let mut waiters = self.external_waiting_futures.borrow_mut();

            // Move all waiters from lock-free queue into internal vector.
            //
            while let Some(wait_one) = self.lock_sync.external_waiting_futures.pop() {
                waiters.push(wait_one);
            }

            if waiters.is_empty() {
                // No waiters, set lock as not acquired.
                //
                self.lock_sync.release();
                None
            } else {
                let mut counter = self.scan_counter.borrow_mut();

                // Start new round if counter is zero.
                //
                if *counter == 0 {
                    *counter = waiters.len();
                }

                // Scan window is min(counter, waiters.len()) - handles new arrivals.
                //
                let scan_window = (*counter).min(waiters.len());

                // Find waiter on least loaded reactor within scan window.
                //
                let best_index = waiters
                    .iter()
                    .take(scan_window)
                    .enumerate()
                    .min_by_key(|(_, signaler)| {
                        signaler
                            .assigned_reactor()
                            .map(|r| r.external_queue_len())
                            .unwrap_or(usize::MAX)
                    })
                    .map(|(i, _)| i)
                    .unwrap();

                // Decrement counter for next release.
                //
                *counter = counter.saturating_sub(1);

                // Remove the best waiter - signal after dropping borrow.
                //
                Some(waiters.swap_remove(best_index))
            }
        };

        // Signal outside the borrow to prevent re-entrancy panic.
        //
        if let Some(signaler) = signaler_to_wake {
            signaler.signal();
        }
    }
}

unsafe impl<T: ?Sized> Send for ReactorAwareLock<T> {}
unsafe impl<T: ?Sized> Sync for ReactorAwareLock<T> {}

// Lock result.
//
pub struct LockResult<'a, T: ?Sized> {
    lock: &'a dyn Lock<T>,
    pub(super) data: &'a mut T,
}

impl<T: ?Sized> Deref for LockResult<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl<T: ?Sized> DerefMut for LockResult<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.data
    }
}

impl<T: ?Sized> Drop for LockResult<'_, T> {
    fn drop(&mut self) {
        self.lock.release();
    }
}
