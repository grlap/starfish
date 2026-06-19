//! Traits for key-value map collections.
//!
//! Defines `MapNode` (node with key and value accessors), `MapCollectionInternal`
//! (crate-private low-level operations), and `MapCollection` (the public safe API
//! for concurrent map collections).
//!
//! Implementations: `SkipList` (via `MapEntry<K,V>`, value-CAS UPDATE), `SortedList` and
//! `SkipTrie` (via `Pair<K,V>`, node-replacement UPDATE), and `SplitOrderedHashMap` (which
//! wraps a `MapCollection` internally).

use crate::guard::Guard;

/// A node in a map collection.
///
pub trait MapNode<K, V> {
    fn key(&self) -> &K;
    fn value(&self) -> Option<&V>;

    /// Deallocate this node.
    ///
    /// # Safety
    /// - The pointer must have been allocated by the collection that created it
    /// - Must only be called once
    /// - Node must not be accessed after this call
    ///
    /// # Default Implementation
    /// Uses `Box::from_raw` which is correct for nodes allocated with `Box::new`.
    /// Nodes with custom allocation must override this method.
    ///
    unsafe fn dealloc_ptr(ptr: *mut Self)
    where
        Self: Sized,
    {
        // SAFETY: caller must ensure ptr was allocated with Box::new
        unsafe { drop(Box::from_raw(ptr)) };
    }
}

/// Crate-private trait for low-level map collection operations.
///
/// These methods operate on raw node pointers and require proper guard
/// discipline (pinned epoch guard) for memory safety. They are **not**
/// exposed to external crates — use the safe [`MapCollection`] API instead.
///
/// Note: This trait is `pub` at the declaration site but lives in a module
/// that is not re-exported from the crate root, making it effectively
/// crate-private. External crates cannot name or import this trait.
pub trait MapCollectionInternal<K, V>
where
    K: Eq,
{
    type Guard: Guard;
    type Node: MapNode<K, V>;

    /// Get the shared guard instance for this collection.
    ///
    /// The shared guard is used for deferred destruction of removed nodes.
    /// All deleted nodes are deferred to this guard and freed when it drops
    /// (when the collection is dropped).
    ///
    fn guard(&self) -> &Self::Guard;

    /// Insert a key-value pair.
    /// Returns the node pointer if inserted, None if key already exists.
    ///
    fn insert_internal(&self, key: K, value: V) -> Option<*mut Self::Node>;

    /// Remove a key and return the node pointer.
    /// Returns the removed node pointer if found, None otherwise.
    ///
    fn remove_internal(&self, key: &K) -> Option<*mut Self::Node>;

    /// Find a key and return the node pointer.
    /// Returns the node pointer if found, None otherwise.
    ///
    fn find_internal(&self, key: &K) -> Option<*mut Self::Node>;

    /// Apply a function on a specific node's value.
    ///
    fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R;

    /// Update a key's value atomically: if the key exists, replace its value and
    /// **retire the old value/node internally** (value-CAS maps `defer_destroy` the
    /// replaced value; node-replacement maps `defer_destroy` the old node). Returns
    /// `true` if the key existed and was updated, `false` otherwise.
    ///
    /// The caller (the default [`update`](MapCollection::update)) must hold a pinned
    /// read guard; this method does not pin its own.
    fn update_internal(&self, key: K, value: V) -> bool;

    /// Get the number of elements.
    ///
    fn len_internal(&self) -> usize;
}

/// Public safe API for concurrent map collections.
///
/// All methods pin the guard internally, ensuring memory safety.
/// Low-level operations are on the crate-private [`MapCollectionInternal`] supertrait.
///
/// # Implementations
///
/// ```text
/// MapCollection (this trait)
///    ↓ implemented by
/// SplitOrderedHashMap              ← Hash-based (K: Hash + Eq)
/// SkipList<MapEntry<K,V>>          ← Order-based (K: Eq + Ord), epoch-safe value-CAS UPDATE
/// SortedList<Pair<K,V>>            ← Order-based (K: Eq + Ord)
/// SkipTrie<Pair<K,V>>              ← Trie-based (K: Eq + Ord + KeyBits + Clone + Default)
/// ```
///
pub trait MapCollection<K, V>: MapCollectionInternal<K, V>
where
    K: Eq,
{
    /// Check if a key exists.
    ///
    fn contains(&self, key: &K) -> bool {
        let _guard = Self::Guard::pin();
        self.find_internal(key).is_some()
    }

    /// Insert a key-value pair.
    /// Returns true if inserted, false if key already exists.
    ///
    fn insert(&self, key: K, value: V) -> bool {
        let _guard = Self::Guard::pin();
        self.insert_internal(key, value).is_some()
    }

    /// Find a key and apply a function to its value.
    ///
    fn find_and_apply<F, R>(&self, key: &K, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
        let _guard = Self::Guard::pin();
        match self.find_internal(key) {
            Some(node) => self.apply_on_internal(node, f),
            None => None,
        }
    }

    /// Get a cloned value for a key.
    ///
    fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        self.find_and_apply(key, |_, v| v.clone())
    }

    /// Remove a key and return its value (cloned).
    ///
    /// Note: The value is cloned because in concurrent contexts other threads
    /// may still be reading the node. The node destruction is deferred via the guard.
    ///
    fn remove(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let _guard = Self::Guard::pin();
        self.remove_internal(key).map(|node| {
            // SAFETY: `node` was returned by `remove_internal`, which logically unlinks
            // the node from the collection. The pinned epoch guard (`_guard`) keeps the
            // node alive until we finish cloning the value. `defer_destroy` schedules
            // deallocation for after all concurrent readers have exited.
            unsafe {
                let value = (*node).value().expect("Removed node has no value").clone();
                self.guard().defer_destroy(node, Self::Node::dealloc_ptr);
                value
            }
        })
    }

    /// Update a key's value atomically.
    ///
    /// Returns `true` if the key existed and was updated, `false` otherwise.
    /// Unlike remove-then-insert, the key is never "missing" during the
    /// update — concurrent readers always see either the old or new value.
    ///
    fn update(&self, key: K, value: V) -> bool {
        let _guard = Self::Guard::pin();
        // Guard pinned for the duration; `update_internal` finds the key, replaces the
        // value, and retires the old value/node internally (value-CAS maps defer the
        // replaced value; node-replacement maps defer the old node).
        self.update_internal(key, value)
    }

    /// Check if the map is empty.
    ///
    fn is_empty(&self) -> bool {
        let _guard = Self::Guard::pin();
        self.len_internal() == 0
    }

    /// Get the number of elements.
    ///
    fn len(&self) -> usize {
        let _guard = Self::Guard::pin();
        self.len_internal()
    }
}
