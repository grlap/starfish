use crate::data_structures::ordered_iterator::OrderedIterator;
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
pub trait SortedCollection<T: Eq + Ord> {
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

    // Methods.
    //

    fn contains(&self, key: &T) -> bool {
        self.find_from_internal(None, key, true).is_some()
    }

    /// Insert a value into the collection.
    /// Returns true if inserted, false if the value already exists.
    ///
    fn insert(&self, value: T) -> bool {
        self.insert_from_internal(value, None).is_some()
    }

    /// Insert a value into the collection after a given position.
    /// Returns true if inserted, false if the value already exists.
    ///
    fn insert_from(&self, position: &Self::NodePosition, key: T) -> bool {
        self.insert_from_internal(key, Some(position)).is_some()
    }

    /// Find a value and apply a function to it, returning the result.
    ///
    fn find_and_apply<F, R>(&self, key: &T, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        match self.find_from_internal(None, key, true) {
            Some(pos) => self.apply_on_internal(pos.node_ptr(), f),
            None => None,
        }
    }

    fn find_from(&self, position: &Self::NodePosition, key: &T) -> Option<Self::NodePosition> {
        self.find_from_internal(Some(position), key, true)
    }

    /// Find a value and apply a function to it from a starting position, returning the result.
    ///
    fn find_from_and_apply<F, R>(&self, position: &Self::NodePosition, key: &T, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        match self.find_from_internal(Some(position), key, true) {
            Some(pos) => self.apply_on_internal(pos.node_ptr(), f),
            None => None,
        }
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
