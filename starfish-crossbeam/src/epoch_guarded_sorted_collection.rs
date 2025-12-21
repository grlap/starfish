use crossbeam_epoch::{self as epoch};
use starfish_core::data_structures::{
    CollectionNode, NodePosition, OrderedIterator, SortedCollection,
};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr;

use crate::epoch_guarded_collection_iter::EpochGuardedCollectionIter;
use crate::guarded_ref::GuardedRef;
use starfish_core::data_structures::SafeSortedCollection;

/// A wrapper that adds epoch-based memory reclamation to any SortedCollection implementation.
///
/// This struct implements `SafeSortedCollection`, providing a safe high-level API
/// for concurrent sorted collections with automatic memory management.
///
/// # Design
///
/// ```text
/// User Code
///    ↓ uses
/// SafeSortedCollection trait        ← Safe, high-level API
///    ↓ implemented by
/// EpochGuardedCollection (this)     ← Epoch-based memory safety
///    ↓ wraps
/// SortedCollection trait            ← Low-level algorithm
///    ↓ implemented by
/// SortedList, SkipList, etc.        ← Actual data structures
/// ```
///
/// # Memory Safety
///
/// - All operations pin an epoch guard for safe memory access
/// - Removed nodes are deferred for destruction via crossbeam-epoch
/// - Values are accessed via `GuardedRef` which bundles the guard with the reference
/// - No raw pointers are exposed to the user
///
/// # Example
///
/// ```rust,ignore
/// use starfish_crossbeam::SafeSortedCollection;
/// use starfish_crossbeam::EpochGuardedCollection;
/// use starfish_core::SortedList;
///
/// let list: Box<dyn SafeSortedCollection<i32>> =
///     Box::new(EpochGuardedCollection::new(SortedList::new()));
///
/// // Safe API - no raw pointers, memory automatically managed
/// list.insert(5);
/// list.insert(10);
/// assert!(list.contains(&5));
///
/// if let Some(val) = list.find(&10) {
///     println!("Found: {}", *val);
/// }
///
/// assert!(list.delete(&5));
/// assert!(!list.contains(&5));
/// ```
///
pub struct EpochGuardedCollection<T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    inner: ManuallyDrop<C>,
    _phantom: PhantomData<T>,
}

impl<T, C> EpochGuardedCollection<T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    /// Create a new epoch-guarded collection wrapping an existing collection.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use starfish_core::SortedList;
    /// use starfish_crossbeam::EpochGuardedCollection;
    ///
    /// let list = SortedList::new();
    /// let guarded = EpochGuardedCollection::new(list);
    /// ```
    pub fn new(collection: C) -> Self {
        EpochGuardedCollection {
            inner: ManuallyDrop::new(collection),
            _phantom: PhantomData,
        }
    }

    /// Internal access to inner collection for iterator implementation.
    pub(crate) fn inner(&self) -> &C {
        &self.inner
    }

    // ========================================================================
    // Additional Methods (beyond SafeSortedCollection trait)
    // ========================================================================
    //
    // These provide "hint-based" operations useful for advanced use cases
    // like split-ordered hash tables.

    /// Insert a value starting from a specific node hint.
    ///
    /// This is useful for data structures like split-ordered hash tables
    /// where you have a bucket head pointer to start the search from.
    ///
    /// Returns `true` if inserted, `false` if the value already exists.
    ///
    pub fn insert_from(&self, start_node_ref: GuardedRef<*mut C::Node>, key: T) -> bool {
        // GuardedRef bundles a guard, but add explicit one for traversing other nodes
        let _guard = epoch::pin();
        let pos = C::NodePosition::from_node(*start_node_ref.get());
        self.inner.insert_from_internal(key, Some(&pos)).is_some()
    }

    /// Delete a value starting from a specific node hint.
    ///
    /// Returns `true` if removed, `false` if not found.
    ///
    pub fn delete_from(&self, start_node_ref: Option<GuardedRef<*mut C::Node>>, key: &T) -> bool {
        let _guard = epoch::pin();
        self.delete_from_internal(start_node_ref, key)
    }

    /// Remove and return a value starting from a specific node hint.
    ///
    /// Returns `Some(value)` if found and removed, `None` if not found.
    ///
    pub fn remove_from(&self, start_node_ref: GuardedRef<*mut C::Node>, key: &T) -> Option<T>
    where
        T: Clone,
    {
        self.remove_from_internal(Some(start_node_ref), key)
    }

    // ========================================================================
    // Internal Helper Methods
    // ========================================================================

    /// Internal implementation for delete operations.
    ///
    /// This handles the actual node removal and deferred destruction.
    ///
    fn delete_from_internal(
        &self,
        start_node_ref: Option<GuardedRef<*mut C::Node>>,
        key: &T,
    ) -> bool {
        let guard = epoch::pin();
        let start_pos = start_node_ref.map(|r| C::NodePosition::from_node(*r.get()));

        if let Some(pos) = self.inner.remove_from_internal(start_pos.as_ref(), key) {
            let node_ptr = pos.node_ptr();
            // Schedule the node for deferred destruction when the epoch advances
            // Use the node's dealloc_ptr to handle custom allocation strategies
            unsafe {
                guard.defer_unchecked(move || {
                    C::Node::dealloc_ptr(node_ptr);
                });
            }
            true
        } else {
            false
        }
    }

    /// Internal implementation for remove operations.
    ///
    /// This clones the value before scheduling the node for destruction.
    ///
    /// Internal implementation for remove operations.
    ///
    /// Uses the enhanced trait to get both node pointer and cloned data
    /// in a single traversal, then performs hint-based removal.
    ///
    /// # Performance
    ///
    /// - **With enhanced trait**: O(log n) find + O(1) to O(log n) hint-based remove
    /// - **vs. old double lookup**: 2 × O(log n)
    ///
    /// # Implementation
    ///
    /// 1. Find the node and clone the value in one traversal
    /// 2. Use the returned node pointer as a hint for faster removal
    /// 3. Schedule the node for deferred destruction
    ///
    /// # Race Condition
    ///
    /// There's a small window where another thread could delete the node
    /// between find and remove. This is handled correctly by returning None.
    ///
    fn remove_from_internal(
        &self,
        start_node_ref: Option<GuardedRef<*mut C::Node>>,
        key: &T,
    ) -> Option<T>
    where
        T: Clone,
    {
        let guard = epoch::pin();
        let start_pos = start_node_ref.map(|r| C::NodePosition::from_node(*r.get()));

        let pos = self.inner.remove_from_internal(start_pos.as_ref(), key)?;
        let removed_node_ptr = pos.node_ptr();

        // Single traversal: find the node and clone the value
        // Returns both the node pointer and the cloned data
        let data = self.inner.apply_on_internal(
            removed_node_ptr,
            |entry| entry.clone(), // Clone while we have access
        );

        // Use node_ptr as a hint for faster removal
        // This should be O(1) for verification in most implementations
        // since we're starting from the exact node we just found
        // Schedule the node for deferred destruction using node's dealloc_ptr
        unsafe {
            guard.defer_unchecked(move || {
                C::Node::dealloc_ptr(removed_node_ptr);
            });
        }

        data
    }
}

// ============================================================================
// SafeSortedCollection Implementation
// ============================================================================

impl<T, C> SafeSortedCollection<T> for EpochGuardedCollection<T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord + Clone,
{
    type GuardedRef<'a>
        = GuardedRef<'a, T>
    where
        Self: 'a,
        T: 'a;

    type Iter<'a>
        = EpochGuardedCollectionIter<'a, T, C>
    where
        Self: 'a;

    fn insert(&self, key: T) -> bool {
        // MUST pin epoch guard! Insert traverses the list via find_position,
        // which accesses existing nodes that could be freed by concurrent deletes.
        let _guard = epoch::pin();
        self.inner.insert_from_internal(key, None).is_some()
    }

    fn delete(&self, key: &T) -> bool {
        let _guard = epoch::pin();
        self.delete_from_internal(None, key)
    }

    fn remove(&self, key: &T) -> Option<T>
    where
        T: Clone,
    {
        self.remove_from_internal(None, key)
    }

    fn update(&self, new_value: T) -> bool {
        let guard = epoch::pin();

        // Use the internal update method which handles forward insertion
        // Returns (old_node_ptr, new_position) - old node is orphaned and needs deferred destruction
        if let Some((old_node_ptr, _new_pos)) = self.inner.update_internal(None, new_value) {
            // Defer destruction of the old node using node's dealloc_ptr
            unsafe {
                guard.defer_unchecked(move || {
                    C::Node::dealloc_ptr(old_node_ptr);
                });
            }
            true
        } else {
            false
        }
    }

    fn contains(&self, key: &T) -> bool {
        let _guard = epoch::pin();
        self.inner.find_from_internal(None, key, true).is_some()
    }

    fn find(&self, key: &T) -> Option<Self::GuardedRef<'_>> {
        let guard = epoch::pin();
        let pos = self.inner.find_from_internal(None, key, true)?;

        // Get a pointer to the data within the node.
        // We already have the node, so use apply_on_internal directly.
        // (find_from_and_apply would use `node` as predecessor and skip it)
        let data_ptr = self
            .inner
            .apply_on_internal(pos.node_ptr(), |entry| entry as *const T)?;

        // Safety: The data_ptr is valid because:
        // 1. We just found the node under epoch protection
        // 2. The GuardedRef will hold the guard, preventing reclamation
        // 3. The lifetime is tied to the GuardedRef
        let data = unsafe { data_ptr.as_ref().unwrap() };

        unsafe { Some(GuardedRef::new(guard, data)) }
    }

    fn find_and_apply<F, R>(&self, key: &T, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        let _guard = epoch::pin();
        self.inner.find_and_apply(key, f)
    }

    fn is_empty(&self) -> bool {
        // Pin guard while accessing the list
        let _guard = epoch::pin();
        self.inner.first_node_internal().is_none()
    }

    fn iter(&self) -> Self::Iter<'_> {
        EpochGuardedCollectionIter::new(self)
    }

    fn iter_from(&self, start_key: &T) -> Self::Iter<'_> {
        // Pin guard for the search phase - iterator will manage its own guard
        let _guard = epoch::pin();

        // First try exact match
        if let Some(pos) = self.inner.find_from_internal(None, start_key, true) {
            return EpochGuardedCollectionIter::from_node(self, Some(pos.node_ptr()));
        }

        // Not found - find predecessor and get its successor
        let pos = self.inner.find_from_internal(None, start_key, false);

        // If we found a predecessor, get its next (which should be > start_key)
        let start_node = if let Some(pred_pos) = pos {
            self.inner.next_node_internal(pred_pos.node_ptr())
        } else {
            // No predecessor found, start from beginning and skip to first >= start_key
            let mut curr = self.inner.first_node_internal();
            while let Some(node) = curr {
                unsafe {
                    if (*node).key() >= start_key {
                        return EpochGuardedCollectionIter::from_node(self, Some(node));
                    }
                }
                curr = self.inner.next_node_internal(node);
            }
            None
        };

        EpochGuardedCollectionIter::from_node(self, start_node)
    }

    fn insert_batch<I>(&self, iter: I) -> usize
    where
        I: OrderedIterator<Item = T>,
    {
        // MUST pin epoch guard! insert_batch traverses the list,
        // accessing nodes that could be freed by concurrent deletes.
        let _guard = epoch::pin();
        self.inner.insert_batch(iter)
    }
}

// Implement Default trait.
//
impl<T, C> Default for EpochGuardedCollection<T, C>
where
    C: SortedCollection<T> + Default,
    T: Eq + Ord,
{
    fn default() -> Self {
        Self::new(C::default())
    }
}

// Safety: EpochGuardedCollection is thread-safe if the inner collection is
unsafe impl<T, C> Send for EpochGuardedCollection<T, C>
where
    C: SortedCollection<T> + Send,
    T: Eq + Ord + Send,
{
}

unsafe impl<T, C> Sync for EpochGuardedCollection<T, C>
where
    C: SortedCollection<T> + Sync,
    T: Eq + Ord + Sync,
{
}

impl<T, C> Drop for EpochGuardedCollection<T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    fn drop(&mut self) {
        let guard = epoch::pin();

        // Safety: We're in Drop, so we own this value
        // We must not access self.inner again after this.
        let inner = unsafe { ptr::read(&*self.inner) };

        // Defer the entire collection's drop to the epoch system
        unsafe {
            guard.defer_unchecked(move || {
                drop(inner); // SortedList::Drop will free all nodes
            });
        }
    }
}

// ============================================================================
// Tests - Unique to EpochGuardedCollection
// ============================================================================
// Note: Common tests are in tests/epoch_guarded_collection_tests.rs

#[cfg(test)]
mod tests {
    use super::*;
    use starfish_core::data_structures::SortedList;

    #[test]
    fn test_find_with_guarded_ref() {
        // Test GuardedRef lifetime and usage pattern
        let guarded = EpochGuardedCollection::<i32, SortedList<_>>::default();

        guarded.insert(5);
        guarded.insert(10);
        guarded.insert(15);

        // Get a guarded reference
        if let Some(guard_ref) = guarded.find(&10) {
            assert_eq!(*guard_ref, 10);

            // Can use the reference while guard_ref is in scope
            let doubled = *guard_ref * 2;
            assert_eq!(doubled, 20);
        } // guard_ref dropped here, guard unpinned
    }

    #[test]
    fn test_memory_reclamation() {
        // Test epoch-based deferred destruction
        let guarded = EpochGuardedCollection::<i32, SortedList<_>>::default();

        // Insert many items
        for i in 0..1000 {
            guarded.insert(i);
        }

        // Delete half of them
        for i in (0..1000).step_by(2) {
            assert!(guarded.delete(&i));
        }

        // Verify deleted items are gone
        for i in (0..1000).step_by(2) {
            assert!(!guarded.contains(&i));
        }

        // Verify remaining items still exist
        for i in (1..1000).step_by(2) {
            assert!(guarded.contains(&i));
        }
    }
}

// ============================================================================
// Implementation Notes
// ============================================================================
//
// # Memory Reclamation Strategy
//
// This implementation uses crossbeam-epoch for safe memory reclamation:
//
// 1. **Epoch Guards**: Every operation that accesses nodes pins an epoch guard
//    - The guard prevents nodes from being freed while in use
//    - Guards are automatically unpinned when dropped
//
// 2. **Deferred Destruction**: When a node is removed:
//    - It's marked as logically deleted in the data structure
//    - `guard.defer_unchecked()` schedules it for physical deletion
//    - The node is actually freed when the epoch advances and no threads
//      hold references to it
//
// 3. **GuardedRef Pattern**: For `find()` operations:
//    - Bundles the epoch guard with a reference to the value
//    - Ensures the reference cannot outlive the guard
//    - Provides safe, ergonomic access to values
//
// # Performance Characteristics
//
// - **Insert**: Lock-free, typically O(log n) for skip lists
// - **Delete/Remove**: Lock-free, O(log n), deferred reclamation
// - **Find/Contains**: Wait-free reads, O(log n)
// - **Memory**: Small overhead per operation (epoch guard)
//
// # Comparison with Other Strategies
//
// - **vs. Hazard Pointers**: Epoch is simpler but may delay reclamation longer
// - **vs. Reference Counting**: Epoch avoids atomic increments on every access
// - **vs. GC**: Epoch is deterministic and has bounded memory overhead
