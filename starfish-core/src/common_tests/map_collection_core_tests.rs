//! Core correctness tests for `MapCollection` implementations.
//!
//! Tests key-value operations (insert, contains, get, remove, update,
//! find_and_apply), length tracking, and concurrent access patterns.
//! Generic over any `MapCollection<K, V>`.

use std::sync::{Arc, Barrier};
use std::thread;

use crate::data_structures::hash::MapCollection;

/// Test insert, contains, get, and duplicate rejection.
pub fn test_insert_contains_get<M: MapCollection<usize, String> + Default + Send + Sync>() {
    let map = M::default();

    assert!(map.insert(1, "one".into()));
    assert!(map.insert(2, "two".into()));
    assert!(map.insert(3, "three".into()));

    // Duplicate insert returns false and preserves original value
    assert!(!map.insert(1, "one_dup".into()));
    assert_eq!(map.get(&1), Some("one".into()));

    assert!(map.contains(&1));
    assert!(map.contains(&2));
    assert!(map.contains(&3));
    assert!(!map.contains(&99));

    assert_eq!(map.get(&1), Some("one".into()));
    assert_eq!(map.get(&2), Some("two".into()));
    assert_eq!(map.get(&3), Some("three".into()));
    assert_eq!(map.get(&99), None);
}

/// Test remove returns correct value and double-remove returns None.
pub fn test_remove_returns_value<M: MapCollection<usize, String> + Default + Send + Sync>() {
    let map = M::default();

    map.insert(10, "ten".into());
    map.insert(20, "twenty".into());

    assert_eq!(map.remove(&10), Some("ten".into()));
    assert!(!map.contains(&10));
    assert_eq!(map.get(&10), None);

    // Double remove returns None
    assert_eq!(map.remove(&10), None);

    // Other key unaffected
    assert_eq!(map.get(&20), Some("twenty".into()));

    // Remove missing key
    assert_eq!(map.remove(&999), None);
}

/// Test len and is_empty accuracy after inserts, duplicates, and removes.
pub fn test_len_accuracy<M: MapCollection<usize, usize> + Default + Send + Sync>() {
    let map = M::default();

    assert!(map.is_empty());
    assert_eq!(map.len(), 0);

    for i in 0..100 {
        map.insert(i, i * i);
    }
    assert!(!map.is_empty());
    assert_eq!(map.len(), 100);

    // Duplicate inserts don't change len
    for i in 0..50 {
        map.insert(i, i);
    }
    assert_eq!(map.len(), 100);

    // Removes decrease len
    for i in 0..30 {
        map.remove(&i);
    }
    assert_eq!(map.len(), 70);
}

/// Test update replaces value, returns false on missing key, and preserves len.
pub fn test_update_value_replacement<M: MapCollection<usize, String> + Default + Send + Sync>() {
    let map = M::default();

    map.insert(1, "old".into());
    assert_eq!(map.get(&1), Some("old".into()));

    assert!(map.update(1, "new".into()));
    assert_eq!(map.get(&1), Some("new".into()));

    // Update missing key returns false
    assert!(!map.update(999, "nope".into()));

    // Len unchanged after update
    assert_eq!(map.len(), 1);
}

/// Test find_and_apply returns computed result or None for missing key.
pub fn test_find_and_apply<M: MapCollection<usize, usize> + Default + Send + Sync>() {
    let map = M::default();

    map.insert(5, 50);
    map.insert(10, 100);

    let doubled = map.find_and_apply(&5, |k, v| {
        assert_eq!(*k, 5);
        v * 2
    });
    assert_eq!(doubled, Some(100));

    let sum = map.find_and_apply(&10, |k, v| *k + v);
    assert_eq!(sum, Some(110));

    let missing = map.find_and_apply(&99, |_, v| v * 2);
    assert_eq!(missing, None);
}

/// Test concurrent insert, get, and remove with overlapping key ranges.
pub fn test_concurrent_insert_get_remove<M>()
where
    M: MapCollection<usize, usize> + Default + Send + Sync + 'static,
{
    let map = Arc::new(M::default());
    let num_threads = 8;
    let num_keys = 500;
    // Barrier ensures all inserts complete before any removes begin
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                // Phase 1: all threads compete on the same key space
                for i in 0..num_keys {
                    map.insert(i, t * num_keys + i);
                }
                barrier.wait();

                // Phase 2: verify all keys reachable (no removes yet)
                for i in 0..num_keys {
                    assert!(map.contains(&i), "thread {} missing key {}", t, i);
                }
                barrier.wait();

                // Phase 3: remove even keys — races with other threads
                for i in (0..num_keys).step_by(2) {
                    map.remove(&i);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // After all threads: odd keys must exist, even keys were removed
    for i in (1..num_keys).step_by(2) {
        assert!(map.contains(&i), "odd key {} missing", i);
    }
    for i in (0..num_keys).step_by(2) {
        assert!(!map.contains(&i), "even key {} should be removed", i);
    }
    assert_eq!(map.len(), num_keys / 2);
}

/// Test concurrent updates preserve all keys and final values are valid.
pub fn test_concurrent_update<M>()
where
    M: MapCollection<usize, usize> + Default + Send + Sync + 'static,
{
    let map = Arc::new(M::default());
    let num_keys = 100;
    let num_threads = 8;
    let updates_per_thread = 1_000;

    // Pre-populate with sentinel outside the write range
    let sentinel = usize::MAX;
    for i in 0..num_keys {
        map.insert(i, sentinel);
    }

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let map = Arc::clone(&map);
            thread::spawn(move || {
                for i in 0..updates_per_thread {
                    let key = i % num_keys;
                    // Result intentionally ignored: SkipList (value-CAS) update always
                    // succeeds for a present key, but SortedList/SkipTrie (node-replacement)
                    // update may transiently return false under contention while a concurrent
                    // update marks the node. This test only drives concurrency.
                    let _ = map.update(key, t * updates_per_thread + i);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // All keys should still exist with values from some thread's write
    assert_eq!(map.len(), num_keys);
    for i in 0..num_keys {
        let val = map
            .get(&i)
            .unwrap_or_else(|| panic!("key {} missing after updates", i));
        // Sentinel (usize::MAX) means no update landed — every key should
        // have been updated at least once. Valid writes are
        // t * updates_per_thread + j where t ∈ [0,8) and j ∈ [0,1000),
        // so the range is [0, 7999].
        assert_ne!(val, sentinel, "key {} was never updated", i);
        assert!(val <= 7999, "key {} has out-of-range value {}", i, val);
    }
}
