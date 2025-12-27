use std::sync::Arc;
use std::thread;

use crate::data_structures::{Ordered, SortedCollection, ordered_from_vec};

/// Test basic insert, contains, and duplicate rejection
pub fn test_basic_operations<C>(collection: &C)
where
    C: SortedCollection<i32>,
{
    // Test insert
    assert!(collection.insert(5));
    assert!(collection.insert(10));
    assert!(collection.insert(3));
    assert!(collection.insert(7));
    assert!(collection.insert(1));

    // Test duplicate rejection
    assert!(!collection.insert(5));
    assert!(!collection.insert(10));

    // Test contains
    assert!(collection.contains(&1));
    assert!(collection.contains(&3));
    assert!(collection.contains(&5));
    assert!(collection.contains(&7));
    assert!(collection.contains(&10));
    assert!(!collection.contains(&2));
    assert!(!collection.contains(&99));

    // Test delete
    assert!(collection.delete(&3));
    assert!(!collection.contains(&3));
    assert!(!collection.delete(&3)); // Already deleted

    // Verify others still present
    assert!(collection.contains(&1));
    assert!(collection.contains(&5));
    assert!(collection.contains(&7));
    assert!(collection.contains(&10));
}

/// Test concurrent insertions from multiple threads
pub fn test_concurrent_operations<C>()
where
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
{
    let collection = Arc::new(C::default());
    let num_threads = 4;
    let items_per_thread = 100;

    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let collection = Arc::clone(&collection);
            thread::spawn(move || {
                for i in 0..items_per_thread {
                    let key = thread_id * items_per_thread + i;
                    collection.insert(key);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    // Verify all inserted
    for i in 0..(num_threads * items_per_thread) {
        assert!(collection.contains(&i), "Missing key: {}", i);
    }
}

/// Test find_and_apply functionality
pub fn test_find_and_apply<C>(collection: &C)
where
    C: SortedCollection<i32>,
{
    collection.insert(5);
    collection.insert(10);
    collection.insert(15);

    let doubled = collection.find_and_apply(&5, |x| x * 2);
    assert_eq!(doubled, Some(10));

    let tripled = collection.find_and_apply(&10, |x| x * 3);
    assert_eq!(tripled, Some(30));

    let missing = collection.find_and_apply(&99, |x| x * 2);
    assert_eq!(missing, None);
}

/// Test concurrent mixed operations (insert, delete, contains)
pub fn test_concurrent_mixed_operations<C>()
where
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
{
    let collection = Arc::new(C::default());
    let num_threads = 6;
    let num_operations = 1000;

    // Pre-populate
    for i in 0..50 {
        collection.insert(i * 3);
    }

    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let collection = Arc::clone(&collection);
            thread::spawn(move || {
                for i in 0..num_operations {
                    let key = (thread_id * num_operations + i) % 500;

                    match i % 5 {
                        0 => {
                            collection.insert(key);
                        }
                        1 => {
                            collection.delete(&key);
                        }
                        2 => {
                            collection.contains(&key);
                        }
                        3 => {
                            let _ = collection.find(&key);
                        }
                        4 => {
                            collection.update(key);
                        }
                        _ => unreachable!(),
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
}

/// Test remove returns value
pub fn test_remove_returns_value<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    collection.insert(42);
    collection.insert(17);
    collection.insert(99);

    // Remove and get value back
    assert_eq!(collection.remove(&42), Some(42));
    assert_eq!(collection.remove(&42), None); // Already removed

    // Other values still present
    assert!(collection.contains(&17));
    assert!(collection.contains(&99));

    // Remove remaining
    assert_eq!(collection.remove(&17), Some(17));
    assert_eq!(collection.remove(&99), Some(99));
}

/// Test find returns reference
pub fn test_find<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    collection.insert(5);
    collection.insert(3);
    collection.insert(7);

    assert_eq!(*collection.find(&3).unwrap(), 3);
    assert_eq!(*collection.find(&5).unwrap(), 5);
    assert_eq!(*collection.find(&7).unwrap(), 7);
    assert!(collection.find(&10).is_none());

    // Delete and verify find returns None
    collection.delete(&5);
    assert!(collection.find(&5).is_none());
}

/// Test sequential insert and delete pattern
pub fn test_sequential_operations<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    // Insert 100 elements
    for i in 0..100 {
        assert!(collection.insert(i));
    }

    // Verify all exist
    for i in 0..100 {
        assert!(collection.contains(&i), "Missing key: {}", i);
    }

    // Remove even numbers
    for i in (0..100).step_by(2) {
        assert!(collection.delete(&i));
    }

    // Verify removed
    for i in (0..100).step_by(2) {
        assert!(!collection.contains(&i), "Should be removed: {}", i);
    }

    // Verify odd numbers still exist
    for i in (1..100).step_by(2) {
        assert!(collection.contains(&i), "Should still exist: {}", i);
    }
}

/// Test high contention on same keys
pub fn test_high_contention<C>()
where
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
{
    let collection = Arc::new(C::default());
    let num_threads = 16;
    let range = 100;

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let collection = Arc::clone(&collection);
            thread::spawn(move || {
                for i in 0..range {
                    collection.insert(i);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    // Should have exactly 'range' items (duplicates rejected)
    for i in 0..range {
        assert!(collection.contains(&i), "Missing key: {}", i);
    }
}

/// Test is_empty functionality
pub fn test_is_empty<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    assert!(collection.is_empty());

    collection.insert(1);
    assert!(!collection.is_empty());

    collection.delete(&1);
    assert!(collection.is_empty());
}

/// Test insert_batch with ordered data
pub fn test_insert_batch_basic<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    // Insert batch of ordered data
    let data = vec![1, 2, 3, 4, 5];
    let ordered = Ordered::new(data.into_iter());
    let inserted = collection.insert_batch(ordered);

    assert_eq!(inserted, 5);

    // Verify all items are present
    for i in 1..=5 {
        assert!(collection.contains(&i), "Missing key: {}", i);
    }
}

/// Test insert_batch with empty iterator
pub fn test_insert_batch_empty<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    let data: Vec<i32> = vec![];
    let ordered = Ordered::new(data.into_iter());
    let inserted = collection.insert_batch(ordered);

    assert_eq!(inserted, 0);
    assert!(collection.is_empty());
}

/// Test insert_batch with duplicates in the batch
pub fn test_insert_batch_with_duplicates<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    // Data with duplicates (still ordered)
    let data = vec![1, 2, 2, 3, 3, 3, 4, 5];
    let ordered = Ordered::new(data.into_iter());
    let inserted = collection.insert_batch(ordered);

    // Only unique values should be inserted
    assert_eq!(inserted, 5);

    // Verify all unique items are present
    for i in 1..=5 {
        assert!(collection.contains(&i), "Missing key: {}", i);
    }

    // Verify to_vec returns exactly unique values (no duplicates stored)
    let items = collection.to_vec();
    assert_eq!(items, vec![1, 2, 3, 4, 5]);
}

/// Test insert_batch with pre-existing items
pub fn test_insert_batch_with_existing<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    // Pre-insert some items
    collection.insert(2);
    collection.insert(4);

    // Batch insert overlapping range
    let data = vec![1, 2, 3, 4, 5];
    let ordered = Ordered::new(data.into_iter());
    let inserted = collection.insert_batch(ordered);

    // Only 3 new items should be inserted (1, 3, 5)
    assert_eq!(inserted, 3);

    // Verify all items are present
    for i in 1..=5 {
        assert!(collection.contains(&i), "Missing key: {}", i);
    }
}

/// Test insert_batch using ordered_from_vec helper
pub fn test_insert_batch_from_unsorted<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    // Use helper that sorts the vec first
    let data = vec![5, 3, 1, 4, 2];
    let ordered = ordered_from_vec(data);
    let inserted = collection.insert_batch(ordered);

    assert_eq!(inserted, 5);

    // Verify all items are present and to_vec is ordered
    let items = collection.to_vec();
    assert_eq!(items, vec![1, 2, 3, 4, 5]);
}

/// Test insert_batch maintains sorted order
pub fn test_insert_batch_preserves_order<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    // Insert first batch
    let data1 = vec![10, 20, 30];
    let ordered1 = Ordered::new(data1.into_iter());
    collection.insert_batch(ordered1);

    // Insert second batch (interleaved)
    let data2 = vec![5, 15, 25, 35];
    let ordered2 = Ordered::new(data2.into_iter());
    collection.insert_batch(ordered2);

    // Verify order via to_vec
    let items = collection.to_vec();
    assert_eq!(items, vec![5, 10, 15, 20, 25, 30, 35]);
}

/// Test insert_batch with large dataset
pub fn test_insert_batch_large<C>()
where
    C: SortedCollection<i32> + Default,
{
    let collection = C::default();

    // Insert 1000 items in batch
    let data: Vec<i32> = (0..1000).collect();
    let ordered = Ordered::new(data.into_iter());
    let inserted = collection.insert_batch(ordered);

    assert_eq!(inserted, 1000);

    // Verify all items are present
    for i in 0..1000 {
        assert!(collection.contains(&i), "Missing key: {}", i);
    }

    // Verify ordered to_vec
    let items = collection.to_vec();
    let expected: Vec<i32> = (0..1000).collect();
    assert_eq!(items, expected);
}

/// Test update operations
pub fn test_update_operations<C>(collection: &C)
where
    C: SortedCollection<i32>,
{
    collection.insert(10);
    collection.insert(20);

    // Update existing value
    assert!(collection.update(10));
    assert!(collection.contains(&10));

    // Update non-existing value
    assert!(!collection.update(30));
    assert!(!collection.contains(&30));

    // Insert new check
    assert!(collection.insert(30));
    assert!(collection.update(30));
}

/// Test concurrent update operations
pub fn test_concurrent_update_operations<C>()
where
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
{
    let collection = Arc::new(C::default());
    let num_threads = 4;
    let items_per_thread = 100;

    // Pre-populate
    for i in 0..(num_threads * items_per_thread) {
        collection.insert(i);
    }

    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let collection = Arc::clone(&collection);
            thread::spawn(move || {
                for i in 0..items_per_thread {
                    let key = thread_id * items_per_thread + i;
                    // Update should succeed as keys exist
                    assert!(collection.update(key));
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
}

/// Test len operations
pub fn test_len_operations<C>(collection: &C)
where
    C: SortedCollection<i32>,
{
    assert_eq!(collection.len(), 0);

    collection.insert(10);
    assert_eq!(collection.len(), 1);

    collection.insert(20);
    assert_eq!(collection.len(), 2);

    collection.insert(10); // Duplicate
    assert_eq!(collection.len(), 2);

    collection.delete(&10);
    assert_eq!(collection.len(), 1);

    collection.delete(&20);
    assert_eq!(collection.len(), 0);

    collection.delete(&30); // Not found
    assert_eq!(collection.len(), 0);
}

/// Test iter operations
pub fn test_iter_operations<C>(collection: &C)
where
    C: SortedCollection<i32> + Default,
{
    use crate::data_structures::SortedCollectionIter;

    collection.insert(10);
    collection.insert(5);
    collection.insert(15);

    // Test basic iteration
    let iter = SortedCollectionIter::new(collection);
    let items: Vec<_> = iter.collect();
    assert_eq!(items, vec![5, 10, 15]);
}
