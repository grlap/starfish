use rstest::rstest;
use serial_test::serial;
use starfish_core::common_tests::sorted_collection_core_tests::*;
use starfish_core::data_structures::SkipList;
use starfish_core::data_structures::SkipTrie;
use starfish_core::data_structures::SortedCollection;
use starfish_core::data_structures::SortedList;
use starfish_crossbeam::EpochGuard;

// Type aliases for cleaner test code
type EpochSortedList = SortedList<i32, EpochGuard>;
type EpochSkipList = SkipList<i32, EpochGuard>;
type EpochSkipTrie = SkipTrie<i32, EpochGuard>;

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_basic<C: SortedCollection<i32>>(#[case] collection: C) {
    test_basic_operations(&collection);
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_concurrent<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_concurrent_operations::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_concurrent_mixed<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_concurrent_mixed_operations::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_find_apply<C: SortedCollection<i32>>(#[case] collection: C) {
    test_find_and_apply::<C>(&collection);
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_sequential<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_sequential_operations::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_contention<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_high_contention::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_find_ref<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_find::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_remove_value<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_remove_returns_value::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_empty<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_is_empty::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_iter<C: SortedCollection<i32> + Default>(#[case] collection: C) {
    test_iter_operations(&collection);
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_range<C: SortedCollection<i32> + Default>(#[case] collection: C) {
    test_range_operations(&collection);
}

// ============================================================================
// insert_batch tests
// ============================================================================

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_batch_basic<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_basic::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_batch_empty<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_empty::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_batch_with_duplicates<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_with_duplicates::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_batch_with_existing<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_with_existing::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_batch_from_unsorted<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_from_unsorted::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_batch_preserves_order<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_preserves_order::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_batch_large<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_large::<C>();
}

// ============================================================================
// High-contention correctness tests (ported from formerly-ignored regression tests)
// ============================================================================

use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_insert_delete_rapid_cycle<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    let list: Arc<C> = Arc::new(C::default());
    let successful_inserts = Arc::new(AtomicUsize::new(0));
    let successful_deletes = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];

    // Multiple threads all operating on key 42
    for _ in 0..4 {
        let list_clone = Arc::clone(&list);
        let inserts = Arc::clone(&successful_inserts);
        let deletes = Arc::clone(&successful_deletes);

        let handle = thread::spawn(move || {
            for _ in 0..1000 {
                if list_clone.insert(42) {
                    inserts.fetch_add(1, Ordering::Relaxed);
                }
                if list_clone.delete(&42) {
                    deletes.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let ins = successful_inserts.load(Ordering::Relaxed);
    let del = successful_deletes.load(Ordering::Relaxed);
    let exists = list.contains(&42);

    // Invariant: inserts - deletes should be 0 or 1
    let diff = ins as i64 - del as i64;
    assert!(
        diff == 0 || diff == 1,
        "Invariant violated: inserts={}, deletes={}, diff={}, exists={}",
        ins,
        del,
        diff,
        exists
    );

    if diff == 1 {
        assert!(exists, "Key should exist when inserts - deletes = 1");
    } else {
        assert!(!exists, "Key should not exist when inserts - deletes = 0");
    }
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn test_partially_linked_node_cleanup<
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
>(
    #[case] _collection: C,
) {
    let list: Arc<C> = Arc::new(C::default());
    let barrier = Arc::new(Barrier::new(3));

    // Pre-populate with even numbers
    for i in 0..50 {
        list.insert(i * 2);
    }

    let list1 = Arc::clone(&list);
    let list2 = Arc::clone(&list);
    let list3 = Arc::clone(&list);
    let barrier1 = Arc::clone(&barrier);
    let barrier2 = Arc::clone(&barrier);
    let barrier3 = Arc::clone(&barrier);

    // Thread 1: Insert odd numbers
    let inserter = thread::spawn(move || {
        barrier1.wait();
        for i in 0..50 {
            list1.insert(i * 2 + 1);
        }
    });

    // Thread 2: Delete all numbers forward
    let deleter1 = thread::spawn(move || {
        barrier2.wait();
        for i in 0..100 {
            list2.delete(&i);
        }
    });

    // Thread 3: Delete all numbers reverse
    let deleter2 = thread::spawn(move || {
        barrier3.wait();
        for i in (0..100).rev() {
            list3.delete(&i);
        }
    });

    inserter.join().unwrap();
    deleter1.join().unwrap();
    deleter2.join().unwrap();

    // Verify remaining list is sorted
    let items: Vec<i32> = list.iter().map(|item| *item).collect();
    for window in items.windows(2) {
        assert!(
            window[0] < window[1],
            "List not sorted after partial link cleanup: {} followed by {}",
            window[0],
            window[1]
        );
    }
}

// ============================================================================
// Benchmark hang reproduction tests
// ============================================================================

type EpochSkipListI64 = SkipList<i64, EpochGuard>;

/// Test that exactly matches the hanging benchmark pattern.
/// Uses SkipList<i64, EpochGuard> - the exact type from the benchmark.
#[test]
#[serial]
fn test_benchmark_hang_reproduction_skiplist() {
    let list: Arc<EpochSkipListI64> = Arc::new(SkipList::default());

    let thread_count = 16;
    let ops_per_thread = 100; // Match benchmark inner loop count

    // Pre-populate with values that each thread will update
    // This is the exact pattern from bench_concurrent_update
    let total_keys = thread_count * 100;
    for i in 0..total_keys {
        list.insert(i as i64);
    }

    let mut handles = vec![];

    for t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            let base = (t * 100) as i64;
            // Each thread repeatedly updates its own range of 100 keys
            for _ in 0..ops_per_thread {
                for j in 0..100i64 {
                    list_clone.update(base + j);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Verify all keys still exist
    for i in 0..total_keys {
        assert!(
            list.contains(&(i as i64)),
            "Key {} should still exist after updates",
            i
        );
    }

    println!("Benchmark hang reproduction test completed successfully");
}

/// High contention version - all threads update same 50 keys
/// This matches bench_high_contention_update_skiplist
#[test]
#[serial]
fn test_benchmark_high_contention_skiplist() {
    let list: Arc<EpochSkipListI64> = Arc::new(SkipList::default());

    let thread_count = 16;
    let ops_per_thread = 1000;
    let key_range = 50i64; // Small range for high contention

    // Pre-populate
    for i in 0..key_range {
        list.insert(i);
    }

    let mut handles = vec![];

    for _ in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            for i in 0..ops_per_thread {
                let key = (i as i64) % key_range;
                list_clone.update(key);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Verify all keys still exist
    for i in 0..key_range {
        assert!(
            list.contains(&i),
            "Key {} should still exist after updates",
            i
        );
    }

    println!("High contention benchmark test completed successfully");
}

/// Extended version that runs longer to increase probability of hitting the hang
#[test]
#[serial]
fn test_benchmark_extended_run_skiplist() {
    let list: Arc<EpochSkipListI64> = Arc::new(SkipList::default());

    let thread_count = 24; // More threads
    let ops_per_thread = 10_000; // More operations
    let key_range = 50i64;

    // Pre-populate
    for i in 0..key_range {
        list.insert(i);
    }

    let mut handles = vec![];

    for _t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            for i in 0..ops_per_thread {
                for _j in 0..key_range {
                    let key = (i as i64) % key_range;
                    list_clone.update(key);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    println!(
        "Extended benchmark run completed - {} threads x {} ops",
        thread_count, ops_per_thread
    );
}

/// Test that forces epoch advancement to trigger memory reclamation
/// This may expose use-after-free or stale pointer bugs
#[test]
#[serial]
fn test_update_with_epoch_pressure() {
    use crossbeam_epoch as epoch;

    let list: Arc<EpochSkipListI64> = Arc::new(SkipList::default());

    let thread_count = 16;
    let ops_per_thread = 1000;
    let key_range = 50i64;

    // Pre-populate
    for i in 0..key_range {
        list.insert(i);
    }

    let mut handles = vec![];

    for t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            for i in 0..ops_per_thread {
                let key = (i as i64) % key_range;
                list_clone.update(key);

                // Force epoch advancement periodically to trigger memory reclamation
                // This increases the chance of exposing use-after-free bugs
                if t == 0 && i % 100 == 0 {
                    // Pin and immediately unpin to help advance epoch
                    let _guard = epoch::pin();
                    // Trigger garbage collection
                    unsafe {
                        epoch::unprotected().flush();
                    }
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    println!("Update with epoch pressure test completed");
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
#[case::skip_trie(EpochSkipTrie::default())]
fn stress_concurrent_update_with_timeout<
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
>(
    #[case] _collection: C,
) {
    use starfish_core::common_tests::sorted_collection_stress_tests::test_concurrent_update_with_timeout;

    test_concurrent_update_with_timeout::<C>();
}
