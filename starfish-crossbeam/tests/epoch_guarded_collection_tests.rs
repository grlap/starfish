use rstest::rstest;
use serial_test::serial;
use starfish_core::common_tests::sorted_collection_core_tests::*;
use starfish_core::data_structures::SkipList;
use starfish_core::data_structures::SortedCollection;
use starfish_core::data_structures::SortedList;
use starfish_crossbeam::EpochGuard;

// Type aliases for cleaner test code
type EpochSortedList = SortedList<i32, EpochGuard>;
type EpochSkipList = SkipList<i32, EpochGuard>;

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_basic<C: SortedCollection<i32>>(#[case] collection: C) {
    test_basic_operations(&collection);
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_concurrent<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_concurrent_operations::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_concurrent_mixed<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_concurrent_mixed_operations::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_find_apply<C: SortedCollection<i32>>(#[case] collection: C) {
    test_find_and_apply::<C>(&collection);
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_sequential<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_sequential_operations::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_contention<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_high_contention::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_find_ref<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_find::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_remove_value<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_remove_returns_value::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_empty<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_is_empty::<C>();
}

// ============================================================================
// insert_batch tests
// ============================================================================

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_batch_basic<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_basic::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_batch_empty<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_empty::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_batch_with_duplicates<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_with_duplicates::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_batch_with_existing<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_with_existing::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_batch_from_unsorted<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_from_unsorted::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_batch_preserves_order<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_preserves_order::<C>();
}

#[rstest]
#[serial]
#[case::sorted_list(EpochSortedList::default())]
#[case::skip_list(EpochSkipList::default())]
fn test_batch_large<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_large::<C>();
}

// ============================================================================
// Benchmark hang reproduction tests
// ============================================================================

use std::sync::Arc;
use std::thread;

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
fn stress_concurrent_update_with_timeout<
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
>(
    #[case] _collection: C,
) {
    use starfish_core::common_tests::sorted_collection_stress_tests::test_concurrent_update_with_timeout;

    test_concurrent_update_with_timeout::<C>();
}
