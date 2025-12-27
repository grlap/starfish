use crate::data_structures::ordered_iterator::OrderedIterator;
use crate::guard::Guard;
use std::marker::PhantomData;
use std::ptr;

pub trait CollectionNode<T> {
    fn key(&self) -> &T;

    /// Deallocate this node.
    ///
    /// # Safety
    /// - The pointer must have been allocated by the collection that created it
    /// - Must only be called once
    /// - Node must not be accessed after this call
    ///
    /// # Default Implementation
    /// Uses `Box::from_raw` which is correct for nodes allocated with `Box::new`.
    /// Nodes with custom allocation (like SkipNode with flexible array members)
    /// must override this method.
    ///
    unsafe fn dealloc_ptr(ptr: *mut Self)
    where
        Self: Sized,
    {
        // SAFETY: caller must ensure ptr was allocated with Box::new
        unsafe { drop(Box::from_raw(ptr)) };
    }
}

/// A position in a sorted collection, containing the node and predecessors at each level.
///
/// This provides:
/// - Access to the node's key and value (like CollectionNode)
/// - Predecessors for efficient O(1) batch operations on sorted data
///
pub trait NodePosition<T>: Clone {
    type Node: CollectionNode<T>;

    /// Get the node pointer at this position (None if empty/invalid)
    fn node(&self) -> Option<*mut Self::Node>;

    /// Get the node pointer, returning null if no node
    fn node_ptr(&self) -> *mut Self::Node {
        self.node().unwrap_or(ptr::null_mut())
    }

    /// Create an empty/invalid position
    fn empty() -> Self;

    /// Create a position from just a node pointer (for backwards compatibility)
    /// Predecessors will be null, so this won't give O(1) batch performance
    fn from_node(node: *mut Self::Node) -> Self;

    /// Check if this position has a valid node
    fn is_valid(&self) -> bool {
        self.node().is_some()
    }

    /// Access key through the position (delegates to node)
    ///
    /// # Safety
    /// Position must be valid (have a non-null node)
    unsafe fn key(&self) -> &T {
        unsafe { (*self.node().unwrap()).key() }
    }
}

/// A trait for sorted collections that maintain elements in order.
///
/// # Type Parameters
///
/// - `T`: The element type (must be `Eq + Ord`)
///
/// # Associated Types
///
/// - `Guard`: The memory reclamation guard type (e.g., `EpochGuard`, `DeferredGuard`)
///
/// # Design
///
/// This trait combines low-level internal methods (for algorithm implementation)
/// with high-level safe methods (for user code). The guard type determines
/// the memory reclamation strategy:
///
/// ```text
/// SortedList<i32, EpochGuard>      - Production: epoch-based reclamation
/// SortedList<i32, DeferredGuard>   - Testing: deferred destruction
/// SkipList<i64, EpochGuard>        - Skip list with epoch-based reclamation
/// ```
///
pub trait SortedCollection<T: Eq + Ord> {
    type Guard: Guard;
    type Node: CollectionNode<T>;
    type NodePosition: NodePosition<T, Node = Self::Node>;

    fn insert_from_internal(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition>;

    fn remove_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
    ) -> Option<Self::NodePosition>;

    /// Finds and returns a node position.
    ///
    fn find_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
        is_match: bool,
    ) -> Option<Self::NodePosition>;

    /// Update a value in the collection by replacing the node.
    ///
    /// This operation atomically replaces a node with a new node containing
    /// the new value. The approach is:
    /// 1. Find the node using the key derived from `new_value`
    /// 2. Create a new node with `new_value`
    /// 3. CAS pred.next from old node to new node
    /// 4. Mark the old node for deletion
    ///
    /// # Arguments
    /// * `position` - Optional starting position for the search
    /// * `new_value` - The new value (used both as search key and replacement value)
    ///
    /// # Returns
    /// * `Some((old_node_ptr, new_position))` - The old node pointer (for deferred destruction)
    ///   and position of the new node if update succeeded
    /// * `None` - If the key was not found
    ///
    /// # Design
    /// For sorted collections, `new_value` serves as both the search key and the
    /// replacement value. For key-value collections like hash maps, `T` is a compound
    /// type (e.g., `Entry<K, V>`) where the key is extracted via `Eq`/`Ord` traits.
    ///
    /// # Memory Management
    /// The caller is responsible for deferring destruction of the old node pointer.
    /// The old node is orphaned (UPDATE-marked) but not freed by this method.
    ///
    fn update_internal(
        &self,
        position: Option<&Self::NodePosition>,
        new_value: T,
    ) -> Option<(*mut Self::Node, Self::NodePosition)>;

    /// Apply a function on specific node
    ///
    fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R;

    /// Get the first data node (skips sentinel).
    /// Returns None if the collection is empty.
    ///
    fn first_node_internal(&self) -> Option<*mut Self::Node>;

    /// Get the next valid (unmarked) node after the given node.
    /// Returns None if there are no more nodes.
    ///
    fn next_node_internal(&self, node: *mut Self::Node) -> Option<*mut Self::Node>;

    /// Insert a sentinel node with maximum height (for skip lists) or standard insert (for lists).
    ///
    /// For skip lists used in split-ordered hash tables, sentinels need maximum height
    /// to enable O(log n) searches within buckets. Regular sorted lists don't have
    /// a concept of "height" so they just perform a normal insert.
    ///
    /// Default implementation delegates to `insert_from_internal`.
    ///
    fn insert_sentinel(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition> {
        self.insert_from_internal(key, position)
    }

    /// Get the shared guard instance for this collection.
    ///
    /// The shared guard is used for deferred destruction of removed nodes.
    /// All deleted nodes are deferred to this guard and freed when it drops
    /// (when the collection is dropped).
    ///
    fn guard(&self) -> &Self::Guard;

    // =========================================================================
    // Safe Public API (uses guard for memory safety)
    // =========================================================================

    /// Insert a value into the collection.
    ///
    /// Returns `true` if the value was inserted, `false` if it already exists.
    ///
    fn insert(&self, key: T) -> bool {
        let _guard = Self::Guard::pin();
        self.insert_from_internal(key, None).is_some()
    }

    /// Remove a value from the collection.
    ///
    /// Returns `true` if the value was removed, `false` if not found.
    ///
    fn delete(&self, key: &T) -> bool {
        let _guard = Self::Guard::pin();
        if let Some(pos) = self.remove_from_internal(None, key) {
            unsafe {
                self.guard()
                    .defer_destroy(pos.node_ptr(), Self::Node::dealloc_ptr);
            }
            true
        } else {
            false
        }
    }

    /// Remove and return the value if it exists.
    ///
    /// Returns `Some(value)` if found and removed, `None` if not found.
    ///
    fn remove(&self, key: &T) -> Option<T>
    where
        T: Clone,
    {
        let _guard = Self::Guard::pin();
        let pos = self.remove_from_internal(None, key)?;
        let node_ptr = pos.node_ptr();

        // Clone the value before scheduling destruction
        let data = self.apply_on_internal(node_ptr, |entry| entry.clone());

        unsafe {
            self.guard()
                .defer_destroy(node_ptr, Self::Node::dealloc_ptr);
        }

        data
    }

    /// Update a value in the collection atomically.
    ///
    /// If the value exists (by `Eq`), replaces the node and returns `true`.
    /// If the value does not exist, returns `false` and does nothing.
    ///
    fn update(&self, new_value: T) -> bool {
        let _guard = Self::Guard::pin();
        if let Some((old_node_ptr, _new_pos)) = self.update_internal(None, new_value) {
            unsafe {
                self.guard()
                    .defer_destroy(old_node_ptr, Self::Node::dealloc_ptr);
            }
            true
        } else {
            false
        }
    }

    /// Check if a value exists in the collection.
    ///
    fn contains(&self, key: &T) -> bool {
        let _guard = Self::Guard::pin();
        self.find_from_internal(None, key, true).is_some()
    }

    /// Find and return a guarded reference to the value.
    ///
    /// Returns a guarded reference that protects the value according to
    /// the guard's memory reclamation strategy.
    ///
    fn find(&self, key: &T) -> Option<<Self::Guard as Guard>::GuardedRef<'_, T>> {
        let _guard = Self::Guard::pin();
        let pos = self.find_from_internal(None, key, true)?;

        let data_ptr = self.apply_on_internal(pos.node_ptr(), |entry| entry as *const T)?;

        // Safety: The guard protects this access, make_ref creates its own guard
        unsafe { Some(Self::Guard::make_ref(data_ptr)) }
    }

    /// Find a value and apply a function to it.
    ///
    fn find_and_apply<F, R>(&self, key: &T, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        let _guard = Self::Guard::pin();
        match self.find_from_internal(None, key, true) {
            Some(pos) => self.apply_on_internal(pos.node_ptr(), f),
            None => None,
        }
    }

    /// Check if the collection is empty.
    ///
    fn is_empty(&self) -> bool {
        let _guard = Self::Guard::pin();
        self.first_node_internal().is_none()
    }

    /// Collects all elements into a Vec.
    ///
    fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        let _guard = Self::Guard::pin();
        let mut result = Vec::new();
        let mut current = self.first_node_internal();
        while let Some(node) = current {
            unsafe {
                result.push((*node).key().clone());
            }
            current = self.next_node_internal(node);
        }
        result
    }

    /// Returns the number of elements in the collection.
    ///
    fn len(&self) -> usize {
        let _guard = Self::Guard::pin();
        let mut count = 0;
        let mut current = self.first_node_internal();
        while current.is_some() {
            count += 1;
            current = self.next_node_internal(current.unwrap());
        }
        count
    }

    /// Insert multiple values in batch from an ordered iterator.
    ///
    /// This uses position-based optimization: each insert uses the previous
    /// insert position (with predecessors at all levels) as the starting point.
    /// For ordered input, this gives O(1) amortized per insert since we only
    /// traverse forward ~1 node per level.
    ///
    /// Returns the number of elements successfully inserted (excludes duplicates).
    ///
    fn insert_batch<I>(&self, iter: I) -> usize
    where
        I: OrderedIterator<Item = T>,
    {
        let _guard = Self::Guard::pin();
        let mut count = 0;
        let mut last_position: Option<Self::NodePosition> = None;

        for value in iter {
            if let Some(pos) = self.insert_from_internal(value, last_position.as_ref()) {
                last_position = Some(pos);
                count += 1;
            }
            // If insert returns None (duplicate), keep using the last successful position
            // as hint - the next value will still be >= that position
        }
        count
    }
}

// ============================================================================
// Iterator Support
// ============================================================================

/// Iterator over a sorted collection with guard protection.
///
/// This iterator holds a read guard for the duration of iteration,
/// ensuring memory safety. For epoch-based guards, this pins the thread.
/// For deferred guards, this is a no-op since the collection's guard
/// provides protection.
///
pub struct SortedCollectionIter<'a, T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    _guard: <C::Guard as Guard>::ReadGuard,
    collection: &'a C,
    current_node: Option<*mut C::Node>,
    _phantom: PhantomData<T>,
}

impl<'a, T, C> SortedCollectionIter<'a, T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    /// Create a new iterator over the collection.
    pub fn new(collection: &'a C) -> Self {
        let guard = C::Guard::pin();
        let first = collection.first_node_internal();
        Self {
            _guard: guard,
            collection,
            current_node: first,
            _phantom: PhantomData,
        }
    }

    /// Create an iterator starting from a specific node.
    pub fn from_node(collection: &'a C, node: Option<*mut C::Node>) -> Self {
        let guard = C::Guard::pin();
        Self {
            _guard: guard,
            collection,
            current_node: node,
            _phantom: PhantomData,
        }
    }
}

impl<'a, T, C> Iterator for SortedCollectionIter<'a, T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord + Clone,
{
    // Note: We return cloned values instead of GuardedRef because the lifetime
    // of GuardedRef would need to be tied to the guard, which we move into the
    // iterator. This is a simpler API that works for most use cases.
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.current_node?;

        // Get next node before returning current
        self.current_node = self.collection.next_node_internal(node);

        // Clone the value (safe because read guard protects access)
        unsafe { Some((*node).key().clone()) }
    }
}
