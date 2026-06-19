//! Core traits for sorted collections.
//!
//! Defines `CollectionNode` (node deallocation), `NodePosition` (cursor into
//! a collection), `SortedCollectionInternal` (crate-private low-level operations),
//! and `SortedCollection` (the public safe API for lock-free sorted data structures).

use crate::data_structures::ordered_iterator::OrderedIterator;
use crate::guard::Guard;
use std::marker::PhantomData;
use std::ops::{Bound, Deref, RangeBounds};
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
pub trait NodePosition<T> {
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
    /// - Position must be valid (have a non-null node).
    /// - The node must **not** be a sentinel/head node. A position returned by
    ///   `find_from_internal(.., is_match = false)` holds the *predecessor*, which is
    ///   the head sentinel whenever the search key precedes all elements — a sentinel's
    ///   key slot is uninitialized (`MaybeUninit`) or `None`, so reading it is UB or a
    ///   panic. Only call `key()` on element positions (exact-match finds, inserts).
    unsafe fn key(&self) -> &T {
        // SAFETY: Caller guarantees the position is valid (node is non-null, not freed,
        // and not a sentinel). The node was returned by a collection lookup and is
        // protected by the guard.
        unsafe { (*self.node().unwrap()).key() }
    }
}

/// Crate-private trait for low-level sorted collection operations.
///
/// These methods operate on raw node pointers and require proper guard
/// discipline (pinned epoch guard) for memory safety. They are **not**
/// exposed to external crates — use the safe [`SortedCollection`] API instead.
///
/// # Safety
///
/// All `unsafe` methods on this trait require the caller to have pinned a
/// read guard (`Self::Guard::pin()`) that remains alive for the duration of
/// the call **and** for as long as any returned raw pointers / positions are
/// used. Calling these methods without a pinned guard can result in
/// use-after-free when epoch-based reclamation is active.
///
/// # Associated Types
///
/// - `Guard`: The memory reclamation guard type (e.g., `EpochGuard`, `DeferredGuard`)
/// - `Node`: The concrete node type stored in the collection
/// - `NodePosition`: A cursor/position within the collection (with predecessor hints)
///
/// Note: This trait is `pub` at the declaration site but lives in the `pub(crate)`
/// `internal` module, making it effectively crate-private. External crates cannot
/// name or import this trait.
pub trait SortedCollectionInternal<T: Eq + Ord> {
    type Guard: Guard;
    type Node: CollectionNode<T>;
    type NodePosition: NodePosition<T, Node = Self::Node>;

    /// # Safety
    /// Caller must hold a pinned read guard for the duration of the call.
    unsafe fn insert_from_internal(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition>;

    /// # Safety
    /// Caller must hold a pinned read guard for the duration of the call.
    unsafe fn remove_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
    ) -> Option<Self::NodePosition>;

    /// Update by NODE REPLACEMENT: replace the node whose key matches `new_value`
    /// with a new node carrying `new_value`, with no window where the key is
    /// absent (forward splice: one CAS marks the old node `UPDATE_MARK` AND links
    /// the same-key replacement after it — readers follow the mark). Returns the
    /// new node's position, or `None` if the key is absent (including when a
    /// concurrent delete wins the race for the carrier).
    ///
    /// RETIREMENT IS INTERNAL for every implementor: the old node is unlinked and
    /// retired by the collection itself — the only party that can prove it
    /// unreachable (`SortedList::unlink_marked_node` loops until confirmed;
    /// `SkipTrie` additionally maintains its x-fast trie; `SkipList` gates its
    /// teardown on tower quiescence via the `linked_height` handshake). Callers
    /// never retire anything.
    ///
    /// Validated epoch-safe for all implementors under EpochGuard + ASan
    /// (`starfish-crossbeam/tests/{sorted_list,skip_trie}_map_epoch.rs`,
    /// `skip_list_pair_epoch.rs`). Forward iteration skips a node's same-key
    /// UPDATE replacement (`next_node_internal`), so a concurrent update never
    /// yields a key twice.
    ///
    /// For maps this is the node-replacement backend (`Pair` payloads — value
    /// inline in the node); `SkipList<MapEntry>` maps use value-CAS instead and
    /// must NOT be driven through this method (it bypasses the tombstone
    /// protocol — same rule as set-API mutation on a value-CAS map). History:
    /// this method was split out to a separate `NodeReplaceUpdate` trait while
    /// `SkipList` could not support node replacement; folded back once the
    /// deferred-physical-delete protocol restored it (2026-06-10).
    ///
    /// # Safety
    /// Caller must hold a pinned read guard for the duration of the call.
    unsafe fn update_internal(
        &self,
        position: Option<&Self::NodePosition>,
        new_value: T,
    ) -> Option<Self::NodePosition>;

    /// Finds and returns a node position.
    ///
    /// When `is_match` is `true`, returns the position of an exact match (or `None`).
    /// When `is_match` is `false`, returns the **predecessor** position — the node
    /// whose successor is `>= key`. This is the insertion point: callers (e.g.,
    /// `SortedCollectionRange`) use `next_node_internal` on the returned position
    /// to reach the first node `>= key`.
    ///
    /// # Safety
    /// Caller must hold a pinned read guard for the duration of the call.
    unsafe fn find_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
        is_match: bool,
    ) -> Option<Self::NodePosition>;

    /// Apply a function on specific node.
    ///
    /// # Safety
    /// Caller must hold a pinned read guard for the duration of the call.
    unsafe fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R;

    /// Get the first data node (skips sentinel).
    /// Returns None if the collection is empty.
    ///
    /// # Safety
    /// Caller must hold a pinned read guard for the duration of the call.
    unsafe fn first_node_internal(&self) -> Option<*mut Self::Node>;

    /// Get the next valid (unmarked) node after the given node.
    /// Returns None if there are no more nodes.
    ///
    /// # Safety
    /// Caller must hold a pinned read guard for the duration of the call.
    unsafe fn next_node_internal(&self, node: *mut Self::Node) -> Option<*mut Self::Node>;

    /// Insert a sentinel node with maximum height (for skip lists) or standard insert (for lists).
    ///
    /// For skip lists used in split-ordered hash tables, sentinels need maximum height
    /// to enable O(log n) searches within buckets. Regular sorted lists don't have
    /// a concept of "height" so they just perform a normal insert.
    ///
    /// Default implementation delegates to `insert_from_internal`.
    ///
    /// # Safety
    /// Caller must hold a pinned read guard for the duration of the call.
    unsafe fn insert_sentinel(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition> {
        // SAFETY: caller holds pinned guard, forwarding precondition.
        unsafe { self.insert_from_internal(key, position) }
    }

    /// Get the shared guard instance for this collection.
    ///
    /// The shared guard is used for deferred destruction of removed nodes.
    /// All deleted nodes are deferred to this guard and freed when it drops
    /// (when the collection is dropped).
    ///
    fn guard(&self) -> &Self::Guard;
}

/// Public safe API for sorted collections.
///
/// All methods pin the guard internally, ensuring memory safety.
/// Low-level operations are on the crate-private [`SortedCollectionInternal`] supertrait.
///
/// # Type Parameters
///
/// - `T`: The element type (must be `Eq + Ord`)
///
/// # Design
///
/// The guard type determines the memory reclamation strategy:
///
/// ```text
/// SortedList<i32, EpochGuard>      - Production: epoch-based reclamation
/// SortedList<i32, DeferredGuard>   - Testing: deferred destruction
/// SkipList<i64, EpochGuard>        - Skip list with epoch-based reclamation
/// ```
///
pub trait SortedCollection<T: Eq + Ord>: SortedCollectionInternal<T> {
    /// Insert a value into the collection.
    ///
    /// Returns `true` if the value was inserted, `false` if it already exists.
    ///
    fn insert(&self, key: T) -> bool {
        let _guard = Self::Guard::pin();
        // SAFETY: guard pinned above.
        unsafe { self.insert_from_internal(key, None) }.is_some()
    }

    /// Remove a value from the collection.
    ///
    /// Returns `true` if the value was removed, `false` if not found.
    ///
    fn delete(&self, key: &T) -> bool {
        let _guard = Self::Guard::pin();
        // SAFETY: guard pinned above.
        if let Some(pos) = unsafe { self.remove_from_internal(None, key) } {
            // SAFETY: `remove_from_internal` logically unlinked this node from the collection,
            // so no new traversals will reach it. The guard defers deallocation until no
            // readers hold references, and `dealloc_ptr` matches the node's allocation method.
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
        // SAFETY: guard pinned above.
        let pos = unsafe { self.remove_from_internal(None, key) }?;
        let node_ptr = pos.node_ptr();

        // Clone the value before scheduling destruction
        // SAFETY: guard pinned above.
        let data = unsafe { self.apply_on_internal(node_ptr, |entry| entry.clone()) };

        // SAFETY: `remove_from_internal` logically unlinked this node, so it is unreachable
        // by new traversals. The value was cloned above while the node was still valid.
        // Deferred destruction ensures no concurrent readers are accessing the node.
        unsafe {
            self.guard()
                .defer_destroy(node_ptr, Self::Node::dealloc_ptr);
        }

        data
    }

    /// "Update" a value — for a set this is a **presence check**.
    ///
    /// A set has no value separate from its elements, so replacing an element with an
    /// `Eq` element is observably a no-op; this returns whether the element is present.
    /// (Key→value maps update via `MapCollection::update`: node-replacement
    /// `update_internal` for `Pair` payloads, value-CAS for `MapEntry`.)
    fn update(&self, new_value: T) -> bool {
        let _guard = Self::Guard::pin();
        // SAFETY: guard pinned above; `find_from_internal` requires it.
        unsafe { self.find_from_internal(None, &new_value, true).is_some() }
    }

    /// Check if a value exists in the collection.
    ///
    fn contains(&self, key: &T) -> bool {
        let _guard = Self::Guard::pin();
        // SAFETY: guard pinned above.
        unsafe { self.find_from_internal(None, key, true) }.is_some()
    }

    /// Find and return a guarded reference to the value.
    ///
    /// Returns a guarded reference that protects the value according to
    /// the guard's memory reclamation strategy.
    ///
    fn find(&self, key: &T) -> Option<<Self::Guard as Guard>::GuardedRef<'_, T>> {
        let _guard = Self::Guard::pin();
        // SAFETY: guard pinned above.
        let pos = unsafe { self.find_from_internal(None, key, true) }?;

        // SAFETY: guard pinned above.
        let data_ptr =
            unsafe { self.apply_on_internal(pos.node_ptr(), |entry| entry as *const T) }?;

        // SAFETY: The guard ensures the node is not reclaimed. `make_ref` wraps the
        // pointer into a guard-protected reference.
        unsafe { Some(Self::Guard::make_ref(data_ptr)) }
    }

    /// Find a value and apply a function to it.
    ///
    fn find_and_apply<F, R>(&self, key: &T, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        let _guard = Self::Guard::pin();
        // SAFETY: guard pinned above.
        match unsafe { self.find_from_internal(None, key, true) } {
            // SAFETY: guard pinned above.
            Some(pos) => unsafe { self.apply_on_internal(pos.node_ptr(), f) },
            None => None,
        }
    }

    /// Check if the collection is empty.
    ///
    fn is_empty(&self) -> bool {
        let _guard = Self::Guard::pin();
        // SAFETY: guard pinned above.
        unsafe { self.first_node_internal() }.is_none()
    }

    /// Returns an iterator over all elements in the collection.
    ///
    /// The iterator returns zero-copy references (`CollectionRef`) that are valid
    /// for the lifetime of the iterator. Values can be accessed without cloning,
    /// or explicitly cloned when needed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Zero-copy iteration
    /// for item in collection.iter() {
    ///     println!("{}", *item);
    /// }
    ///
    /// // Collect with explicit clones
    /// let values: Vec<T> = collection.iter().map(|item| item.clone()).collect();
    /// ```
    fn iter(&self) -> SortedCollectionIter<'_, T, Self>
    where
        Self: Sized,
    {
        SortedCollectionIter::new(self)
    }

    /// Returns an iterator over elements in the specified range.
    ///
    /// The range can be any type implementing `RangeBounds<T>`:
    /// - `5..10` - from 5 (inclusive) to 10 (exclusive)
    /// - `5..=10` - from 5 to 10 (both inclusive)
    /// - `..10` - all elements less than 10
    /// - `5..` - all elements >= 5
    /// - `..` - all elements (equivalent to `iter()`)
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Range query
    /// for item in collection.range(100..200) {
    ///     println!("{}", *item);
    /// }
    ///
    /// // Open-ended range
    /// for item in collection.range(500..) {
    ///     println!("{}", *item);
    /// }
    /// ```
    fn range<R>(&self, range: R) -> SortedCollectionRange<'_, T, Self, R>
    where
        Self: Sized,
        R: RangeBounds<T>,
    {
        SortedCollectionRange::new(self, range)
    }

    /// Collects all elements into a Vec.
    ///
    /// This is a convenience method equivalent to:
    /// `collection.iter().map(|item| (*item).clone()).collect()`
    ///
    fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
        Self: Sized,
    {
        self.iter().map(|item| (*item).clone()).collect()
    }

    /// Returns the number of elements in the collection.
    ///
    fn len(&self) -> usize {
        let _guard = Self::Guard::pin();
        let mut count = 0;
        // SAFETY: guard pinned above.
        let mut current = unsafe { self.first_node_internal() };
        while current.is_some() {
            count += 1;
            // SAFETY: guard pinned above.
            current = unsafe { self.next_node_internal(current.unwrap()) };
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
            // SAFETY: guard pinned above.
            if let Some(pos) = unsafe { self.insert_from_internal(value, last_position.as_ref()) } {
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

/// A zero-copy reference borrowed from a collection iterator.
///
/// This reference is protected by the iterator's guard and can be used
/// without cloning the underlying data. Users can process the reference
/// directly or clone it if they need to keep the value beyond iteration.
///
/// # Example
///
/// ```ignore
/// for item_ref in collection.iter() {
///     // Zero-copy access via Deref
///     println!("{}", *item_ref);
///
///     // Clone the underlying value if needed
///     if should_keep(&item_ref) {
///         let owned = (*item_ref).clone();  // or item_ref.get().clone()
///         saved_items.push(owned);
///     }
/// }
/// ```
pub struct CollectionRef<'a, T> {
    reference: &'a T,
}

impl<'a, T> CollectionRef<'a, T> {
    /// Create a new collection reference.
    ///
    /// # Safety
    /// The reference must remain valid for lifetime 'a, which is
    /// guaranteed by the iterator's guard.
    unsafe fn new(reference: &'a T) -> Self {
        CollectionRef { reference }
    }

    /// Get the inner reference.
    pub fn get(&self) -> &T {
        self.reference
    }
}

impl<T> Deref for CollectionRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.reference
    }
}

impl<T: std::fmt::Display> std::fmt::Display for CollectionRef<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reference)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for CollectionRef<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CollectionRef({:?})", self.reference)
    }
}

/// Iterator over a sorted collection with guard protection.
///
/// This iterator holds a read guard for the duration of iteration,
/// ensuring memory safety. Returns zero-copy references that are valid
/// for the lifetime of the iterator.
///
/// For epoch-based guards, this pins the thread once for the entire iteration.
/// For deferred guards, this is a no-op since the collection's guard provides protection.
///
/// # Zero-Copy Design
///
/// The iterator returns `CollectionRef<'a, T>` which borrows from the iterator.
/// Users can access values without cloning, or explicitly clone when needed:
///
/// ```ignore
/// for item in list.iter() {
///     process(&*item);        // Zero-copy access
///     let copy = item.clone(); // Explicit clone if needed
/// }
/// ```
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
        // SAFETY: guard pinned above and stored in Self for the iterator's lifetime.
        let first = unsafe { collection.first_node_internal() };
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

    /// Refresh the iterator's read guard without ending the traversal.
    pub fn repin(&mut self) {
        C::Guard::repin(&mut self._guard);
    }
}

impl<'a, T, C> Iterator for SortedCollectionIter<'a, T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord + 'a,
{
    type Item = CollectionRef<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.current_node?;

        // SAFETY: `_guard: ReadGuard` is held for the iterator's lifetime, preventing
        // node reclamation. `next_node_internal` and `(*node).key()` are safe to call
        // because the guard is pinned and the collection is borrowed for 'a.
        unsafe {
            // Get next node before returning current
            self.current_node = self.collection.next_node_internal(node);

            // Return zero-copy reference protected by iterator's guard
            Some(CollectionRef::new((*node).key()))
        }
    }
}

// ============================================================================
// Range Iterator Support
// ============================================================================

/// Iterator over a range of elements in a sorted collection.
///
/// This iterator is created by the `range()` method and supports all
/// standard Rust range types (`a..b`, `a..=b`, `..b`, `a..`, etc.).
///
/// Like `SortedCollectionIter`, this returns zero-copy `CollectionRef` values.
///
/// # Example
///
/// ```ignore
/// // Iterate from 100 to 200 (exclusive)
/// for item in collection.range(100..200) {
///     println!("{}", *item);
/// }
///
/// // Iterate from 50 onwards
/// for item in collection.range(50..) {
///     println!("{}", *item);
/// }
/// ```
pub struct SortedCollectionRange<'a, T, C, R>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
    R: RangeBounds<T>,
{
    _guard: <C::Guard as Guard>::ReadGuard,
    collection: &'a C,
    current_node: Option<*mut C::Node>,
    range: R,
    done: bool,
    /// False until the first in-range element is yielded: the start-node
    /// resolution may conservatively begin at the collection's first node (see
    /// `new`), so `next()` filters the start bound until it lands in range —
    /// sorted order makes the filter dead weight afterwards.
    started: bool,
    _phantom: PhantomData<T>,
}

impl<'a, T, C, R> SortedCollectionRange<'a, T, C, R>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
    R: RangeBounds<T>,
{
    /// Create a new range iterator.
    pub fn new(collection: &'a C, range: R) -> Self {
        let guard = C::Guard::pin();

        // SAFETY: guard pinned above and stored in Self for the iterator's lifetime.
        // All internal calls below are protected by this guard.
        let start_node = unsafe {
            // Find the starting node based on the range's start bound
            match range.start_bound() {
                Bound::Included(start_key) => {
                    // Find first node >= start_key
                    // Try exact match first
                    let pos = collection.find_from_internal(None, start_key, true);
                    match pos {
                        Some(p) => p.node(),
                        None => {
                            // No exact match, find insertion position and take next node
                            match collection.find_from_internal(None, start_key, false) {
                                Some(p) => {
                                    if let Some(node) = p.node() {
                                        // Position points to predecessor, get next node
                                        collection.next_node_internal(node)
                                    } else {
                                        None
                                    }
                                }
                                // No predecessor POSITION at all: the start key
                                // precedes every element (SkipTrie returns None
                                // here — a below-minimum `range(start..)` used to
                                // yield an EMPTY range; SortedList panicked on the
                                // sentinel key read instead), or the
                                // predecessor was transiently deleted. Start from
                                // the first node; `next()`'s start-bound filter
                                // discards anything below the bound.
                                None => collection.first_node_internal(),
                            }
                        }
                    }
                }
                Bound::Excluded(start_key) => {
                    // Find first node > start_key
                    // First try to find exact match
                    let pos = collection.find_from_internal(None, start_key, true);
                    match pos {
                        Some(p) => {
                            // Found exact match, skip to next
                            if let Some(node) = p.node() {
                                collection.next_node_internal(node)
                            } else {
                                None
                            }
                        }
                        None => {
                            // No exact match, find insertion position and take next node
                            match collection.find_from_internal(None, start_key, false) {
                                Some(p) => {
                                    if let Some(node) = p.node() {
                                        collection.next_node_internal(node)
                                    } else {
                                        None
                                    }
                                }
                                // See the Included arm: no predecessor position ⇒
                                // start from the first node, filtered in `next()`.
                                None => collection.first_node_internal(),
                            }
                        }
                    }
                }
                Bound::Unbounded => collection.first_node_internal(),
            }
        };

        Self {
            _guard: guard,
            collection,
            current_node: start_node,
            range,
            done: false,
            started: false,
            _phantom: PhantomData,
        }
    }

    /// Refresh the range iterator's read guard without ending the traversal.
    pub fn repin(&mut self) {
        C::Guard::repin(&mut self._guard);
    }
}

impl<'a, T, C, R> Iterator for SortedCollectionRange<'a, T, C, R>
where
    C: SortedCollection<T>,
    T: Eq + Ord + 'a,
    R: RangeBounds<T>,
{
    type Item = CollectionRef<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            let node = self.current_node?;

            // SAFETY: `node` is a valid pointer returned by `first_node_internal` or
            // `next_node_internal`. The `_guard: ReadGuard` prevents reclamation while
            // the guard is held, and `&'a C` ensures the collection outlives the iterator.
            // `next_node_internal` is safe to call because the guard is held.
            unsafe {
                let key = (*node).key();

                // Check if we've exceeded the end bound
                match self.range.end_bound() {
                    Bound::Included(end) => {
                        if key > end {
                            self.done = true;
                            return None;
                        }
                    }
                    Bound::Excluded(end) => {
                        if key >= end {
                            self.done = true;
                            return None;
                        }
                    }
                    Bound::Unbounded => {}
                }

                // Start-bound filter, live only until the first yield: the start
                // node may conservatively be the collection's FIRST node when no
                // predecessor position existed (below-minimum start, or a
                // transiently deleted predecessor). Sorted order means once one
                // element is in range, all later ones are too.
                if !self.started {
                    let below = match self.range.start_bound() {
                        Bound::Included(start) => key < start,
                        Bound::Excluded(start) => key <= start,
                        Bound::Unbounded => false,
                    };
                    if below {
                        // SAFETY: guard is held by iterator.
                        self.current_node = self.collection.next_node_internal(node);
                        continue;
                    }
                    self.started = true;
                }

                // Advance to next node
                // SAFETY: guard is held by iterator.
                self.current_node = self.collection.next_node_internal(node);

                // Return current item
                return Some(CollectionRef::new(key));
            }
        }
    }
}
