use std::ops::Deref;

use crate::data_structures::ordered_iterator::OrderedIterator;

/// High-level safe API for sorted collections.
///
/// This trait defines the safe, user-facing API for sorted collections.
/// It should be implemented by wrappers that add memory safety guarantees
/// (like epoch-based reclamation or hazard pointers) rather than by the
/// raw data structures themselves.
///
/// # Design Philosophy
///
/// ```text
/// User Code
///    ↓ uses
/// SafeSortedCollection (this trait) ← Safe, high-level API
///    ↓ implemented by
/// EpochGuardedCollection           ← Epoch-based memory safety
/// HazardPointerCollection          ← Hazard pointer-based safety
/// RcCollection                     ← Reference counting
///    ↓ wraps
/// SortedCollection                 ← Low-level algorithm trait
///    ↓ implemented by
/// SortedList, SkipList, etc.       ← Actual data structures
/// ```
///
/// # Implementations
///
/// Different memory reclamation strategies implement this trait:
///
/// - **EpochGuardedCollection**: Uses crossbeam-epoch for deferred reclamation
/// - **HazardPointerCollection**: Uses hazard pointers for bounded reclamation
/// - **RcCollection**: Uses reference counting (simpler, less concurrent)
///
/// All provide the same safe API but with different performance characteristics.
///
/// # Example
///
/// ```rust,ignore
/// use starfish_crossbeam::SafeSortedCollection;
/// use starfish_crossbeam::EpochGuardedCollection;
/// use starfish_core::SortedList;
///
/// // Create an epoch-protected list
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
///
/// // Iteration support
/// for item in list.iter() {
///     println!("Item: {}", *item);
/// }
/// ```
///
pub trait SafeSortedCollection<T: Eq + Ord> {
    /// The guard type that protects references to values.
    ///
    /// Different implementations use different guard types based on their
    /// memory reclamation strategy:
    ///
    /// - **EpochGuardedCollection**: `EpochGuardedRef<'g, T>` containing `epoch::Guard`
    /// - **HazardPointerCollection**: `HazardGuardedRef<'g, T>` containing hazard pointer
    /// - **RcCollection**: `Arc<T>` (no guard needed, reference counted)
    ///
    /// The type must implement `Deref<Target = T>` so users can access the value
    /// transparently via the dereference operator.
    ///
    type GuardedRef<'a>: Deref<Target = T>
    where
        Self: 'a,
        T: 'a;

    /// Iterator type that yields guarded references to elements.
    ///
    /// The iterator holds appropriate guards to ensure memory safety during
    /// iteration over lock-free data structures.
    ///
    type Iter<'a>: Iterator<Item = Self::GuardedRef<'a>>
    where
        Self: 'a,
        T: 'a;

    /// Insert a value into the collection.
    ///
    /// Returns `true` if the value was inserted, `false` if it already exists.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// assert!(collection.insert(5));
    /// assert!(!collection.insert(5)); // Duplicate
    /// ```
    fn insert(&self, key: T) -> bool;

    /// Remove a value from the collection.
    ///
    /// Returns `true` if the value was removed, `false` if not found.
    /// The value is discarded (not returned).
    ///
    /// Use this when you don't need the value back and want better performance.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// collection.insert(5);
    /// assert!(collection.delete(&5));
    /// assert!(!collection.delete(&5)); // Already deleted
    /// ```
    fn delete(&self, key: &T) -> bool;

    /// Remove and return the value if it exists.
    ///
    /// Returns `Some(value)` if found and removed, `None` if not found.
    /// Requires `T: Clone` to extract the value before reclamation.
    ///
    /// Use this when you need the value back.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// collection.insert(5);
    /// assert_eq!(collection.remove(&5), Some(5));
    /// assert_eq!(collection.remove(&5), None); // Already removed
    /// ```
    fn remove(&self, key: &T) -> Option<T>
    where
        T: Clone;

    /// Update a value in the collection atomically.
    ///
    /// If the value exists (by `Eq`), replaces the node and returns `true`.
    /// If the value does not exist, returns `false` and does nothing.
    ///
    /// This operation is atomic: concurrent readers will see either the old
    /// node or the new node, never a torn read.
    ///
    /// # Implementation
    ///
    /// Uses forward insertion with UPDATE_MARK:
    /// 1. Find the existing node B using `new_value` as key
    /// 2. Create new node B' with the same value
    /// 3. CAS B.next[0] to point to B' with UPDATE_MARK
    /// 4. Readers that encounter UPDATE_MARK follow the pointer to B'
    ///
    /// # Use Case
    ///
    /// For sorted SET (where T is both key and value), this atomically replaces
    /// the node - useful for refreshing node metadata or ensuring atomic
    /// presence without a delete-then-insert gap.
    ///
    /// For key-value collections, `T` is a compound type (e.g., `Entry<K, V>`)
    /// where the key determines lookup and the value can differ.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// collection.insert(5);
    /// assert!(collection.update(5));   // Node with 5 is atomically replaced
    /// assert!(!collection.update(99)); // 99 doesn't exist, returns false
    /// assert!(collection.contains(&5)); // 5 still exists
    /// ```
    fn update(&self, new_value: T) -> bool;

    /// Check if a value exists in the collection.
    ///
    /// Returns `true` if the value exists, `false` otherwise.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// collection.insert(5);
    /// assert!(collection.contains(&5));
    /// assert!(!collection.contains(&10));
    /// ```
    fn contains(&self, key: &T) -> bool;

    /// Find and return a guarded reference to the value.
    ///
    /// Returns a `Self::GuardedRef<'_>` which protects the value according to
    /// the implementation's memory reclamation strategy. The guard ensures the
    /// value cannot be freed while the reference exists.
    ///
    /// Returns `None` if the value is not found.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// collection.insert(5);
    ///
    /// if let Some(val) = collection.find(&5) {
    ///     println!("Found: {}", *val);  // Deref works via Deref trait
    ///     let doubled = *val * 2;
    ///     assert_eq!(doubled, 10);
    /// } // Guard automatically released here
    /// ```
    fn find(&self, key: &T) -> Option<Self::GuardedRef<'_>>;

    /// Find a value and apply a function to it.
    ///
    /// This is the most flexible way to access values. The closure is executed
    /// while the value is protected, and the result is returned.
    ///
    /// Returns `Some(result)` if the value exists, `None` otherwise.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// collection.insert(5);
    ///
    /// // Clone the value
    /// let value = collection.find_and_apply(&5, |x| x.clone());
    /// assert_eq!(value, Some(5));
    ///
    /// // Perform computation
    /// let doubled = collection.find_and_apply(&5, |x| x * 2);
    /// assert_eq!(doubled, Some(10));
    ///
    /// // Check properties
    /// let is_even = collection.find_and_apply(&5, |x| x % 2 == 0);
    /// assert_eq!(is_even, Some(false));
    /// ```
    fn find_and_apply<F, R>(&self, key: &T, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R;

    /// Check if the collection is empty.
    ///
    /// Returns `true` if there are no elements, `false` otherwise.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let collection = EpochGuardedCollection::new(SortedList::new());
    /// assert!(collection.is_empty());
    ///
    /// collection.insert(5);
    /// assert!(!collection.is_empty());
    /// ```
    fn is_empty(&self) -> bool;

    // ========================================================================
    // Iteration Methods
    // ========================================================================

    /// Returns an iterator over all elements in sorted order.
    ///
    /// The iterator yields guarded references that ensure memory safety.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// collection.insert(5);
    /// collection.insert(3);
    /// collection.insert(7);
    ///
    /// for item in collection.iter() {
    ///     println!("{}", *item);  // Prints: 3, 5, 7
    /// }
    /// ```
    fn iter(&self) -> Self::Iter<'_>;

    /// Returns an iterator starting from the given key (inclusive).
    ///
    /// If the key exists, iteration starts from that element.
    /// If not, iteration starts from the first element greater than the key.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// for i in [10, 20, 30, 40, 50] {
    ///     collection.insert(i);
    /// }
    ///
    /// // Start from 25 - gets 30, 40, 50
    /// for item in collection.iter_from(&25) {
    ///     println!("{}", *item);
    /// }
    /// ```
    fn iter_from(&self, start_key: &T) -> Self::Iter<'_>;

    /// Collects all elements into a Vec.
    ///
    /// Convenience method that clones all elements.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// collection.insert(3);
    /// collection.insert(1);
    /// collection.insert(2);
    ///
    /// let vec = collection.to_vec();
    /// assert_eq!(vec, vec![1, 2, 3]);
    /// ```
    fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.iter().map(|r| (*r).clone()).collect()
    }

    /// Returns the number of elements in the collection.
    ///
    /// Note: This may be O(n) for some implementations.
    fn len(&self) -> usize {
        self.iter().count()
    }

    // ========================================================================
    // Batch Operations
    // ========================================================================

    /// Insert multiple values in batch from an ordered iterator.
    ///
    /// This uses splice-based optimization: each insert uses the previous
    /// insert position as a starting hint. For ordered input, this gives
    /// O(log D) per insert where D is the distance from the previous position,
    /// instead of O(log N) per insert.
    ///
    /// For perfectly sequential inserts (1, 2, 3, 4...), D = 1, giving
    /// O(1) amortized per insert.
    ///
    /// Returns the number of elements successfully inserted (excludes duplicates).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use starfish_core::data_structures::ordered_iterator::Ordered;
    ///
    /// let data = vec![1, 2, 3, 4, 5];
    /// let ordered = Ordered::new(data.into_iter());
    ///
    /// let inserted = collection.insert_batch(ordered);
    /// assert_eq!(inserted, 5);
    /// ```
    fn insert_batch<I>(&self, iter: I) -> usize
    where
        I: OrderedIterator<Item = T>;
}

// ============================================================================
// Generic Functions Using SafeSortedCollection
// ============================================================================

/// Example of a generic function that works with any safe sorted collection.
///
/// This demonstrates how the trait enables writing code that works with
/// any memory reclamation strategy.
///
#[allow(dead_code)]
fn example_generic_function<C>(collection: &C)
where
    C: SafeSortedCollection<i32>,
{
    // Insert some values
    collection.insert(1);
    collection.insert(5);
    collection.insert(10);

    // Find and process
    if let Some(val) = collection.find(&5) {
        println!("Found: {}", *val);
    }

    // Delete
    collection.delete(&1);

    // Remove and use value
    if let Some(removed) = collection.remove(&10) {
        println!("Removed: {}", removed);
    }
}

// ============================================================================
// Testing Utilities
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A test that works with any SafeSortedCollection implementation.
    ///
    /// This can be used to test both EpochGuardedCollection and
    /// HazardPointerCollection with the same test code.
    ///
    #[allow(dead_code)]
    pub fn test_safe_collection_basic<C>(collection: &C)
    where
        C: SafeSortedCollection<i32>,
    {
        // Test insert
        assert!(collection.insert(5));
        assert!(collection.insert(10));
        assert!(collection.insert(3));
        assert!(!collection.insert(5)); // Duplicate

        // Test contains
        assert!(collection.contains(&3));
        assert!(collection.contains(&5));
        assert!(collection.contains(&10));
        assert!(!collection.contains(&99));

        // Test find_and_apply
        let doubled = collection.find_and_apply(&5, |x| x * 2);
        assert_eq!(doubled, Some(10));

        // Test delete
        assert!(collection.delete(&3));
        assert!(!collection.contains(&3));
        assert!(!collection.delete(&3)); // Already deleted

        // Test remove
        assert_eq!(collection.remove(&5), Some(5));
        assert_eq!(collection.remove(&5), None); // Already removed

        // Check remaining
        assert!(collection.contains(&10));
        assert!(!collection.is_empty());
    }
}

// ============================================================================
// Implementation Notes
// ============================================================================
//
// To implement SafeSortedCollection for a new memory reclamation strategy:
//
// 1. Create a wrapper struct:
//    ```rust
//    pub struct YourStrategyCollection<T, C>
//    where
//        C: SortedCollection<T>,
//        T: Eq + Ord,
//    {
//        inner: C,
//        _phantom: PhantomData<T>,
//    }
//    ```
//
// 2. Implement SafeSortedCollection:
//    ```rust
//    impl<T, C> SafeSortedCollection<T> for YourStrategyCollection<T, C>
//    where
//        C: SortedCollection<T>,
//        T: Eq + Ord + Clone,
//    {
//        fn insert(&self, key: T) -> bool {
//            // Your memory protection logic
//            self.inner.insert_from_internal(key, None).is_some()
//        }
//
//        fn delete(&self, key: &T) -> bool {
//            // Your memory protection + deferred destruction
//        }
//
//        // ... implement other methods
//    }
//    ```
//
// 3. Example strategies:
//    - EpochGuardedCollection: crossbeam-epoch
//    - HazardPointerCollection: hazard pointers
//    - RcCollection: Arc-based (simpler, less concurrent)
//    - GcCollection: Hypothetical GC-based
