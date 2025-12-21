use crossbeam_epoch::{self as epoch};
use starfish_core::data_structures::hash::{HashMapCollection, HashMapNode, SafeHashMapCollection};
use std::hash::Hash;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr;

use crate::guarded_ref::GuardedRef;

/// A wrapper that adds epoch-based memory reclamation to any HashMapCollection.
///
/// This struct implements `SafeHashMapCollection`, providing a safe high-level API
/// for concurrent hash maps with automatic memory management.
///
/// # Design
///
/// ```text
/// User Code
///    ↓ uses
/// SafeHashMapCollection trait     ← Safe, high-level API
///    ↓ implemented by
/// EpochGuardedHashMap (this)      ← Epoch-based memory safety
///    ↓ wraps
/// HashMapCollection trait         ← Low-level algorithm
///    ↓ implemented by
/// SplitOrderedHashMap             ← Actual data structure
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
/// use starfish_crossbeam::EpochGuardedHashMap;
/// use starfish_core::SplitOrderedHashMap;
///
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
pub struct EpochGuardedHashMap<K, V, C>
where
    C: HashMapCollection<K, V>,
    K: Hash + Eq,
{
    inner: ManuallyDrop<C>,
    _phantom: PhantomData<(K, V)>,
}

impl<K, V, C> EpochGuardedHashMap<K, V, C>
where
    C: HashMapCollection<K, V>,
    K: Hash + Eq,
{
    /// Create a new epoch-guarded hash map wrapping an existing collection.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use starfish_core::SplitOrderedHashMap;
    /// use starfish_crossbeam::EpochGuardedHashMap;
    ///
    /// let map = SplitOrderedHashMap::new();
    /// let guarded = EpochGuardedHashMap::new(map);
    /// ```
    pub fn new(collection: C) -> Self {
        EpochGuardedHashMap {
            inner: ManuallyDrop::new(collection),
            _phantom: PhantomData,
        }
    }
}

// ============================================================================
// SafeHashMapCollection Implementation
// ============================================================================

impl<K, V, C> SafeHashMapCollection<K, V> for EpochGuardedHashMap<K, V, C>
where
    C: HashMapCollection<K, V>,
    K: Hash + Eq + Clone,
    V: Clone,
{
    type GuardedRef<'a>
        = GuardedRef<'a, V>
    where
        Self: 'a,
        V: 'a;

    fn insert(&self, key: K, value: V) -> bool {
        // MUST pin epoch guard! Insert traverses the list,
        // accessing existing nodes that could be freed by concurrent deletes.
        let _guard = epoch::pin();
        self.inner.insert_internal(key, value).is_some()
    }

    fn remove(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let guard = epoch::pin();

        let node_ptr = self.inner.remove_internal(key)?;

        // Clone the value - we can't take it because other threads may still be reading.
        let value = unsafe {
            (*node_ptr)
                .value()
                .expect("Removed node has no value")
                .clone()
        };

        // Schedule the node for deferred destruction using node's dealloc_ptr
        unsafe {
            guard.defer_unchecked(move || {
                C::Node::dealloc_ptr(node_ptr);
            });
        }

        Some(value)
    }

    fn contains(&self, key: &K) -> bool {
        let _guard = epoch::pin();
        self.inner.find_internal(key).is_some()
    }

    fn get(&self, key: &K) -> Option<Self::GuardedRef<'_>> {
        let guard = epoch::pin();
        let node_ptr = self.inner.find_internal(key)?;

        // Get a pointer to the value within the node.
        let value_ptr = self
            .inner
            .apply_on_internal(node_ptr, |_, v| v as *const V)?;

        // Safety: The value_ptr is valid because:
        // 1. We just found the node under epoch protection
        // 2. The GuardedRef will hold the guard, preventing reclamation
        // 3. The lifetime is tied to the GuardedRef
        let value = unsafe { value_ptr.as_ref().unwrap() };

        unsafe { Some(GuardedRef::new(guard, value)) }
    }

    fn find_and_apply<F, R>(&self, key: &K, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
        let _guard = epoch::pin();
        self.inner.find_and_apply(key, f)
    }

    fn is_empty(&self) -> bool {
        let _guard = epoch::pin();
        self.inner.is_empty()
    }

    fn len(&self) -> usize {
        self.inner.len_internal()
    }
}

// Implement Default trait.
impl<K, V, C> Default for EpochGuardedHashMap<K, V, C>
where
    C: HashMapCollection<K, V> + Default,
    K: Hash + Eq,
{
    fn default() -> Self {
        Self::new(C::default())
    }
}

// Safety: EpochGuardedHashMap is thread-safe if the inner collection is
unsafe impl<K, V, C> Send for EpochGuardedHashMap<K, V, C>
where
    C: HashMapCollection<K, V> + Send,
    K: Hash + Eq + Send,
    V: Send,
{
}

unsafe impl<K, V, C> Sync for EpochGuardedHashMap<K, V, C>
where
    C: HashMapCollection<K, V> + Sync,
    K: Hash + Eq + Sync,
    V: Sync,
{
}

impl<K, V, C> Drop for EpochGuardedHashMap<K, V, C>
where
    C: HashMapCollection<K, V>,
    K: Hash + Eq,
{
    fn drop(&mut self) {
        let guard = epoch::pin();

        // Safety: We're in Drop, so we own this value
        // We must not access self.inner again after this.
        let inner = unsafe { ptr::read(&*self.inner) };

        // Defer the entire collection's drop to the epoch system
        unsafe {
            guard.defer_unchecked(move || {
                drop(inner); // Collection's Drop will free all nodes
            });
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use starfish_core::data_structures::SplitOrderedHashMap;

    #[test]
    fn test_basic_operations() {
        let map: EpochGuardedHashMap<i32, String, SplitOrderedHashMap<i32, String>> =
            EpochGuardedHashMap::new(SplitOrderedHashMap::new());

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

    #[test]
    fn test_concurrent_operations() {
        use std::sync::Arc;
        use std::thread;

        let map = Arc::new(EpochGuardedHashMap::new(SplitOrderedHashMap::new()));
        let num_threads = 8;
        let ops_per_thread = 1000;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    for i in 0..ops_per_thread {
                        let key = t * ops_per_thread + i;
                        map.insert(key, key * 2);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(map.len(), num_threads * ops_per_thread);

        // Verify all values
        for t in 0..num_threads {
            for i in 0..ops_per_thread {
                let key = t * ops_per_thread + i;
                assert_eq!(map.get(&key).map(|v| *v), Some(key * 2));
            }
        }
    }

    #[test]
    fn test_memory_reclamation() {
        let map: EpochGuardedHashMap<i32, String, SplitOrderedHashMap<i32, String>> =
            EpochGuardedHashMap::new(SplitOrderedHashMap::new());

        // Insert many items
        for i in 0..1000 {
            map.insert(i, format!("value_{}", i));
        }

        // Delete half of them
        for i in (0..1000).step_by(2) {
            assert_eq!(map.remove(&i), Some(format!("value_{}", i)));
        }

        // Verify deleted items are gone
        for i in (0..1000).step_by(2) {
            assert!(!map.contains(&i));
        }

        // Verify remaining items still exist
        for i in (1..1000).step_by(2) {
            assert!(map.contains(&i));
        }
    }
}
