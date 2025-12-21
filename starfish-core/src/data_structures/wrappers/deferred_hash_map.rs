use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::Mutex;

use crate::data_structures::hash::{HashMapCollection, HashMapNode, SafeHashMapCollection};

/// Hash map wrapper that defers node destruction until drop.
///
/// Use for testing only. For production, use `EpochGuardedHashMap`.
///
pub struct DeferredHashMap<K, V, C>
where
    C: HashMapCollection<K, V>,
    K: Hash + Eq,
{
    inner: C,
    deferred_nodes: Mutex<Vec<*mut C::Node>>,
    _phantom: PhantomData<(K, V)>,
}

impl<K, V, C> DeferredHashMap<K, V, C>
where
    C: HashMapCollection<K, V>,
    K: Hash + Eq,
{
    pub fn new(collection: C) -> Self {
        DeferredHashMap {
            inner: collection,
            deferred_nodes: Mutex::new(Vec::new()),
            _phantom: PhantomData,
        }
    }

    /// Internal access to inner collection.
    pub(crate) fn inner(&self) -> &C {
        &self.inner
    }
}

/// Guarded reference to a value in a DeferredHashMap.
pub struct DeferredHashMapGuardedRef<'a, V> {
    data: &'a V,
}

impl<'a, V> DeferredHashMapGuardedRef<'a, V> {
    /// Creates a new guarded reference from a data reference.
    pub fn new(data: &'a V) -> Self {
        DeferredHashMapGuardedRef { data }
    }
}

impl<'a, V> Deref for DeferredHashMapGuardedRef<'a, V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl<K, V, C> SafeHashMapCollection<K, V> for DeferredHashMap<K, V, C>
where
    C: HashMapCollection<K, V>,
    K: Hash + Eq,
{
    type GuardedRef<'a>
        = DeferredHashMapGuardedRef<'a, V>
    where
        Self: 'a,
        V: 'a;

    fn insert(&self, key: K, value: V) -> bool {
        self.inner.insert_internal(key, value).is_some()
    }

    fn remove(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let node = self.inner.remove_internal(key)?;

        // Clone the value - we can't take it because other threads may still be reading.
        let value = unsafe { (*node).value().expect("Removed node has no value").clone() };

        // Defer node deletion.
        self.deferred_nodes.lock().unwrap().push(node);

        Some(value)
    }

    fn contains(&self, key: &K) -> bool {
        self.inner.contains(key)
    }

    fn get(&self, key: &K) -> Option<Self::GuardedRef<'_>> {
        let node = self.inner.find_internal(key)?;
        unsafe {
            let value_ref = (*node).value()?;
            Some(DeferredHashMapGuardedRef::new(value_ref))
        }
    }

    fn find_and_apply<F, R>(&self, key: &K, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
        self.inner.find_and_apply(key, f)
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K, V, C> Default for DeferredHashMap<K, V, C>
where
    C: HashMapCollection<K, V> + Default,
    K: Hash + Eq,
{
    fn default() -> Self {
        Self::new(C::default())
    }
}

impl<K, V, C> Drop for DeferredHashMap<K, V, C>
where
    C: HashMapCollection<K, V>,
    K: Hash + Eq,
{
    fn drop(&mut self) {
        // Free all deferred nodes using proper deallocation.
        let nodes = self.deferred_nodes.get_mut().unwrap();
        for node in nodes.drain(..) {
            unsafe {
                C::Node::dealloc_ptr(node);
            }
        }
    }
}

// Thread safety
unsafe impl<K, V, C> Send for DeferredHashMap<K, V, C>
where
    C: HashMapCollection<K, V> + Send,
    K: Hash + Eq + Send,
    V: Send,
{
}

unsafe impl<K, V, C> Sync for DeferredHashMap<K, V, C>
where
    C: HashMapCollection<K, V> + Sync,
    K: Hash + Eq + Sync,
    V: Sync,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_structures::SplitOrderedHashMap;

    #[test]
    fn test_deferred_hash_map_basic() {
        let map = DeferredHashMap::new(SplitOrderedHashMap::new());

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
    fn test_deferred_hash_map_deferred_destruction() {
        let map = DeferredHashMap::new(SplitOrderedHashMap::new());

        // Insert and remove many items
        for i in 0..100 {
            map.insert(i, format!("value_{}", i));
        }

        for i in 0..100 {
            assert_eq!(map.remove(&i), Some(format!("value_{}", i)));
        }

        assert!(map.is_empty());
        // Nodes are deferred - will be freed when map is dropped
    }
}
