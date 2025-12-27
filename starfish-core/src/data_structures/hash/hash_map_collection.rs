use std::hash::Hash;

use crate::guard::Guard;

/// A node in a hash map collection.
///
pub trait HashMapNode<K, V> {
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

/// A trait for hash map collections.
///
/// This is the low-level API that raw hash map implementations provide.
/// Memory-safe wrappers (like EpochGuardedHashMap) wrap this trait
/// and implement SafeHashMap.
///
/// # Design Philosophy
///
/// ```text
/// User Code
///    ↓ uses
/// SafeHashMap                      ← Safe, high-level API
///    ↓ implemented by
/// EpochGuardedHashMap              ← Epoch-based memory safety
/// DeferredHashMap                  ← Deferred destruction (testing)
///    ↓ wraps
/// HashMapCollection (this trait)   ← Low-level algorithm trait
///    ↓ implemented by
/// SplitOrderedHashMap              ← Actual data structure
/// ```
///
pub trait HashMapCollection<K, V>
where
    K: Hash + Eq,
{
    type Guard: Guard;
    type Node: HashMapNode<K, V>;

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

    /// Get the number of elements.
    ///
    fn len_internal(&self) -> usize;

    // ========================================================================
    // Convenience methods
    // ========================================================================

    /// Check if a key exists.
    ///
    fn contains(&self, key: &K) -> bool {
        self.find_internal(key).is_some()
    }

    /// Insert a key-value pair.
    /// Returns true if inserted, false if key already exists.
    ///
    fn insert(&self, key: K, value: V) -> bool {
        self.insert_internal(key, value).is_some()
    }

    /// Find a key and apply a function to its value.
    ///
    fn find_and_apply<F, R>(&self, key: &K, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
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
        self.remove_internal(key).map(|node| unsafe {
            let value = (*node).value().expect("Removed node has no value").clone();
            self.guard().defer_destroy(node, Self::Node::dealloc_ptr);
            value
        })
    }

    /// Check if the map is empty.
    ///
    fn is_empty(&self) -> bool {
        self.len_internal() == 0
    }

    /// Get the number of elements.
    ///
    fn len(&self) -> usize {
        self.len_internal()
    }
}
