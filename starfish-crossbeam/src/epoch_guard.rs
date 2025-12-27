//! Epoch-based guard implementation using crossbeam-epoch.
//!
//! This module provides `EpochGuard`, an implementation of the `Guard` trait
//! using crossbeam-epoch for memory reclamation.
//!
//! # Design
//!
//! `EpochGuard` is a zero-sized type that schedules destruction using the global
//! epoch collector. Collections parameterized with `EpochGuard` get epoch-based
//! memory reclamation:
//!
//! ```text
//! SortedList<i32, EpochGuard>
//!     │
//!     └── Uses crossbeam-epoch for memory safety
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use starfish_core::{SortedList, SortedCollection};
//! use starfish_crossbeam::EpochGuard;
//!
//! let list: SortedList<i32, EpochGuard> = SortedList::new();
//!
//! // All operations use epoch-based reclamation
//! list.insert(42);
//! list.insert(17);
//!
//! if let Some(val) = list.find(&42) {
//!     println!("Found: {}", *val);
//! }
//!
//! list.delete(&42);
//! ```

use crossbeam_epoch::{self as epoch, Guard as CrossbeamGuard};
use starfish_core::guard::Guard;
use std::ops::Deref;

/// Epoch-based memory reclamation guard.
///
/// This guard uses crossbeam-epoch to safely defer node destruction.
/// Nodes are not freed until all threads have advanced past the epoch
/// in which they were removed.
///
/// # Design
///
/// Unlike `DeferredGuard` which stores pending destructions, `EpochGuard`
/// is a zero-sized type that schedules destruction using the global epoch
/// collector. This allows it to be stored in collections without making
/// them non-Send/non-Sync.
///
/// When `defer_destroy` is called, it:
/// 1. Pins the current thread to the current epoch
/// 2. Schedules the destruction to run after all threads have advanced
/// 3. Unpins immediately (the destruction is managed globally)
///
/// # Performance
///
/// - **Pin overhead**: Very low (thread-local check)
/// - **Reclamation**: Batched, amortized O(1) per node
/// - **Memory**: May accumulate during long-running operations
///
/// # Thread Safety
///
/// `EpochGuard` is `Send` and `Sync` - it can be safely shared across threads.
///
#[derive(Clone, Copy, Default)]
pub struct EpochGuard {
    // Zero-sized - all state is in the global epoch collector
}

impl EpochGuard {
    /// Create a new epoch guard.
    ///
    /// This is a no-op since EpochGuard is stateless - the actual pinning
    /// happens when operations are performed.
    pub fn new() -> Self {
        EpochGuard {}
    }
}

/// A reference protected by an epoch guard.
///
/// This struct bundles an epoch guard with a reference to ensure the reference
/// cannot outlive the guard. When the `EpochRef` is dropped, the guard is
/// unpinned, allowing epoch-based garbage collection to proceed.
///
pub struct EpochRef<'a, T> {
    _guard: CrossbeamGuard,
    reference: &'a T,
}

impl<'a, T> EpochRef<'a, T> {
    /// Create a new epoch-protected reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The reference points to valid data
    /// - The guard protects access to that data
    /// - The data won't be freed while the guard is pinned
    ///
    pub(crate) unsafe fn new(guard: CrossbeamGuard, reference: &'a T) -> Self {
        EpochRef {
            _guard: guard,
            reference,
        }
    }

    /// Get the inner reference.
    pub fn get(&self) -> &T {
        self.reference
    }
}

impl<T> Deref for EpochRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.reference
    }
}

impl<T: std::fmt::Display> std::fmt::Display for EpochRef<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reference)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for EpochRef<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EpochRef({:?})", self.reference)
    }
}

// EpochRef is Send/Sync if T is
unsafe impl<T: Send> Send for EpochRef<'_, T> {}
unsafe impl<T: Sync> Sync for EpochRef<'_, T> {}

// EpochGuard is Send and Sync since it's stateless (zero-sized)
unsafe impl Send for EpochGuard {}
unsafe impl Sync for EpochGuard {}

impl Guard for EpochGuard {
    type GuardedRef<'a, T: 'a> = EpochRef<'a, T>;

    /// For EpochGuard, ReadGuard is an actual crossbeam epoch guard
    /// that pins the current thread for the duration of reads.
    type ReadGuard = CrossbeamGuard;

    fn pin() -> Self::ReadGuard {
        epoch::pin()
    }

    unsafe fn defer_destroy<N>(&self, node: *mut N, dealloc: unsafe fn(*mut N)) {
        // Pin the current thread, schedule destruction, then unpin
        // The destruction will happen after all threads have advanced past
        // the current epoch
        let guard = epoch::pin();
        unsafe {
            guard.defer_unchecked(move || {
                dealloc(node);
            });
        }
        // guard dropped here - unpins the thread
    }

    unsafe fn make_ref<'a, T: 'a>(ptr: *const T) -> Self::GuardedRef<'a, T> {
        // Create a new guard for the reference
        // This ensures the reference stays valid
        let new_guard = epoch::pin();
        unsafe { EpochRef::new(new_guard, &*ptr) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_epoch_guard_basic() {
        // EpochGuard::default() creates a zero-sized guard for storage
        let guard = EpochGuard::default();

        // Create some test data
        let boxed = Box::new(42i32);
        let ptr = Box::into_raw(boxed);

        // Defer destruction - this pins internally
        unsafe {
            guard.defer_destroy(ptr, |p| {
                drop(Box::from_raw(p));
            });
        }

        // Node scheduled for reclamation via global epoch collector
    }

    #[test]
    fn test_epoch_ref() {
        let value = 42;
        // EpochGuard::pin() returns a CrossbeamGuard for read protection
        let _guard = EpochGuard::pin();

        unsafe {
            let guarded = EpochGuard::make_ref(&value);
            assert_eq!(*guarded, 42);
            assert_eq!(guarded.get(), &42);
        }
    }

    #[test]
    fn test_epoch_ref_display() {
        let value = 42;
        let _guard = EpochGuard::pin();

        unsafe {
            let guarded = EpochGuard::make_ref(&value);
            assert_eq!(format!("{}", guarded), "42");
            assert_eq!(format!("{:?}", guarded), "EpochRef(42)");
        }
    }

    #[test]
    fn test_multiple_deferred() {
        // Multiple defer_destroy calls via the stored guard
        let guard = EpochGuard::default();

        let boxed1 = Box::new(1i32);
        let boxed2 = Box::new(2i32);
        let ptr1 = Box::into_raw(boxed1);
        let ptr2 = Box::into_raw(boxed2);

        unsafe {
            guard.defer_destroy(ptr1, |p| drop(Box::from_raw(p)));
            guard.defer_destroy(ptr2, |p| drop(Box::from_raw(p)));
        }
    }
}
