use std::hash::Hash;
use std::ops::Deref;

/// High-level safe API for hash map collections.
///
/// This trait defines the safe, user-facing API for hash maps.
/// It should be implemented by wrappers that add memory safety guarantees
/// (like epoch-based reclamation or hazard pointers) rather than by the
/// raw data structures themselves.
///
/// # Design Philosophy
///
/// ```text
/// User Code
///    ↓ uses
/// SafeHashMapCollection (this trait) ← Safe, high-level API
///    ↓ implemented by
/// EpochGuardedHashMap                ← Epoch-based memory safety
/// DeferredHashMap                    ← Deferred destruction (testing)
///    ↓ wraps
/// HashMapCollection                  ← Low-level algorithm trait
///    ↓ implemented by
/// SplitOrderedHashMap                ← Actual data structure
/// ```
///
/// # Example
///
/// ```rust,ignore
/// use starfish_crossbeam::EpochGuardedHashMap;
/// use starfish_core::SplitOrderedHashMap;
///
/// // Create an epoch-protected hash map
/// let map = EpochGuardedHashMap::new(SplitOrderedHashMap::new());
///
/// // Safe API - no raw pointers, memory automatically managed
/// map.insert("key1", "value1");
/// map.insert("key2", "value2");
/// assert!(map.contains(&"key1"));
///
/// if let Some(val) = map.get(&"key1") {
///     println!("Found: {}", *val);
/// }
///
/// assert_eq!(map.remove(&"key1"), Some("value1"));
/// assert!(!map.contains(&"key1"));
/// ```
///
pub trait SafeHashMapCollection<K, V>
where
    K: Hash + Eq,
{
    /// The guard type that protects references to values.
    ///
    /// Different implementations use different guard types based on their
    /// memory reclamation strategy:
    ///
    /// - **EpochGuardedHashMap**: Contains `epoch::Guard`
    /// - **DeferredHashMap**: Simple reference (deferred until drop)
    ///
    /// The type must implement `Deref<Target = V>` so users can access the value
    /// transparently via the dereference operator.
    ///
    type GuardedRef<'a>: Deref<Target = V>
    where
        Self: 'a,
        V: 'a;

    /// Insert a key-value pair into the map.
    ///
    /// Returns `true` if the pair was inserted, `false` if the key already exists.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// assert!(map.insert("key", "value"));
    /// assert!(!map.insert("key", "other")); // Key exists
    /// ```
    fn insert(&self, key: K, value: V) -> bool;

    /// Remove a key and return its value (cloned).
    ///
    /// Returns `Some(value)` if found and removed, `None` if not found.
    ///
    /// Note: The value is cloned because in concurrent contexts other threads
    /// may still be reading the node. For this reason, V must implement Clone.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// map.insert("key", "value");
    /// assert_eq!(map.remove(&"key"), Some("value"));
    /// assert_eq!(map.remove(&"key"), None); // Already removed
    /// ```
    fn remove(&self, key: &K) -> Option<V>
    where
        V: Clone;

    /// Check if a key exists in the map.
    ///
    /// Returns `true` if the key exists, `false` otherwise.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// map.insert("key", "value");
    /// assert!(map.contains(&"key"));
    /// assert!(!map.contains(&"other"));
    /// ```
    fn contains(&self, key: &K) -> bool;

    /// Get a guarded reference to the value for a key.
    ///
    /// Returns a `Self::GuardedRef<'_>` which protects the value according to
    /// the implementation's memory reclamation strategy. The guard ensures the
    /// value cannot be freed while the reference exists.
    ///
    /// Returns `None` if the key is not found.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// map.insert("key", 42);
    ///
    /// if let Some(val) = map.get(&"key") {
    ///     println!("Found: {}", *val);  // Deref works via Deref trait
    ///     let doubled = *val * 2;
    ///     assert_eq!(doubled, 84);
    /// } // Guard automatically released here
    /// ```
    fn get(&self, key: &K) -> Option<Self::GuardedRef<'_>>;

    /// Find a key and apply a function to its key-value pair.
    ///
    /// This is the most flexible way to access values. The closure is executed
    /// while the value is protected, and the result is returned.
    ///
    /// Returns `Some(result)` if the key exists, `None` otherwise.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// map.insert("count", 5);
    ///
    /// // Clone the value
    /// let value = map.find_and_apply(&"count", |_, v| v.clone());
    /// assert_eq!(value, Some(5));
    ///
    /// // Perform computation
    /// let doubled = map.find_and_apply(&"count", |_, v| v * 2);
    /// assert_eq!(doubled, Some(10));
    ///
    /// // Access both key and value
    /// let formatted = map.find_and_apply(&"count", |k, v| format!("{}: {}", k, v));
    /// assert_eq!(formatted, Some("count: 5".to_string()));
    /// ```
    fn find_and_apply<F, R>(&self, key: &K, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R;

    /// Check if the map is empty.
    ///
    /// Returns `true` if there are no elements, `false` otherwise.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let map = DeferredHashMap::new(SplitOrderedHashMap::new());
    /// assert!(map.is_empty());
    ///
    /// map.insert("key", "value");
    /// assert!(!map.is_empty());
    /// ```
    fn is_empty(&self) -> bool;

    /// Returns the number of elements in the map.
    ///
    fn len(&self) -> usize;
}

// ============================================================================
// Testing Utilities
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A test that works with any SafeHashMapCollection implementation.
    ///
    /// This can be used to test both EpochGuardedHashMap and
    /// DeferredHashMap with the same test code.
    ///
    #[allow(dead_code)]
    pub fn test_safe_hash_map_basic<C>(map: &C)
    where
        C: SafeHashMapCollection<i32, String>,
    {
        // Test insert
        assert!(map.insert(1, "one".to_string()));
        assert!(map.insert(2, "two".to_string()));
        assert!(map.insert(3, "three".to_string()));
        assert!(!map.insert(1, "uno".to_string())); // Duplicate key

        // Test contains
        assert!(map.contains(&1));
        assert!(map.contains(&2));
        assert!(map.contains(&3));
        assert!(!map.contains(&99));

        // Test get
        assert_eq!(map.get(&1).map(|v| v.clone()), Some("one".to_string()));
        assert!(map.get(&99).is_none());

        // Test find_and_apply
        let len = map.find_and_apply(&2, |_, v| v.len());
        assert_eq!(len, Some(3)); // "two".len() == 3

        // Test remove
        assert_eq!(map.remove(&3), Some("three".to_string()));
        assert!(!map.contains(&3));
        assert_eq!(map.remove(&3), None); // Already removed

        // Check remaining
        assert!(map.contains(&1));
        assert!(map.contains(&2));
        assert!(!map.is_empty());
        assert_eq!(map.len(), 2);
    }
}
