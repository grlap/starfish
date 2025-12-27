//! Guard trait for memory reclamation strategies.
//!
//! This module defines the `Guard` trait that abstracts over different memory
//! reclamation strategies (epoch-based, hazard pointers, deferred, etc.).
//!
//! # Design
//!
//! The `Guard` trait enables collections to be generic over their memory
//! reclamation strategy:
//!
//! ```text
//! SortedCollection<T, G: Guard>
//!     │
//!     ├── SortedList<T, EpochGuard>      (production)
//!     ├── SortedList<T, DeferredGuard>   (testing)
//!     └── SortedList<T, HazardGuard>     (alternative)
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use starfish_core::{SortedList, Guard};
//! use starfish_crossbeam::EpochGuard;
//!
//! // Production: epoch-based reclamation
//! let list: SortedList<i32, EpochGuard> = SortedList::new();
//! list.insert(42);
//!
//! // Testing: deferred destruction
//! let test_list: SortedList<i32, DeferredGuard> = SortedList::new();
//! ```

mod deferred_guard;

use std::ops::Deref;

pub use deferred_guard::{DeferredGuard, DeferredRef};

/// A memory reclamation guard that protects concurrent access to nodes.
///
/// Different implementations provide different trade-offs:
///
/// - **EpochGuard**: Low overhead, batched reclamation (crossbeam-epoch)
/// - **DeferredGuard**: Simple, defers all destruction until guard drops (testing)
/// - **HazardGuard**: Bounded memory, per-pointer protection (future)
///
/// # Safety Contract
///
/// Implementations must ensure:
/// 1. Nodes passed to `defer_destroy` are not freed until it's safe
/// 2. `GuardedRef` keeps the referenced data valid for its lifetime
///
/// # Design Note
///
/// Guards are stored in collections and must be `Send + Sync`. The guard
/// stored in a collection is used for deferred destruction scheduling.
/// Actual thread pinning (for epoch-based guards) happens per-operation,
/// not when the guard is created.
///
pub trait Guard: Sized + Default + Send + Sync {
    /// A reference protected by a guard of this type.
    ///
    /// Must implement `Deref<Target = T>` for transparent access.
    /// The reference owns its guard and is valid for lifetime `'a`.
    ///
    type GuardedRef<'a, T: 'a>: Deref<Target = T>;

    /// An active guard that protects reads for its lifetime.
    ///
    /// For epoch-based guards, this holds an actual pinned `crossbeam_epoch::Guard`.
    /// For deferred guards, this can be a unit type `()` since protection
    /// is provided by the collection's stored guard.
    ///
    type ReadGuard: Sized;

    /// Pin an active read guard.
    ///
    /// This creates a guard that protects all node reads until dropped.
    /// Use this for iteration or batch read operations.
    ///
    /// Note: This is different from `Default::default()` which creates
    /// a guard for storage in collections (for deferred destruction).
    ///
    fn pin() -> Self::ReadGuard;

    /// Schedule a node for deferred destruction.
    ///
    /// The node will be deallocated when it's safe (no readers).
    ///
    /// # Safety
    ///
    /// - `node` must be a valid pointer previously allocated by the collection
    /// - `node` must be unlinked from the collection (not reachable by traversal)
    /// - `dealloc` must be the correct deallocation function for `node`
    ///
    unsafe fn defer_destroy<N>(&self, node: *mut N, dealloc: unsafe fn(*mut N));

    /// Create a guarded reference from a raw pointer.
    ///
    /// This creates a new guard internally to protect the reference.
    /// The returned `GuardedRef` owns the protection mechanism.
    ///
    /// # Safety
    ///
    /// - `ptr` must point to valid data protected by some guard
    /// - The data must remain valid for lifetime `'a`
    ///
    unsafe fn make_ref<'a, T: 'a>(ptr: *const T) -> Self::GuardedRef<'a, T>;
}
