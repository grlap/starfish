use rstest::rstest;
use serial_test::serial;
use starfish_core::common_tests::sorted_collection_core_tests::*;
use starfish_core::data_structures::{DeferredCollection, SkipList, SortedCollection, SortedList};
use starfish_crossbeam::epoch_guarded_sorted_collection::EpochGuardedCollection;

// Trait for type-level parametrization
trait TestSortedCollection {
    type CollectionType: SortedCollection<i32> + Default + Send + Sync + 'static;
}

// Marker types for each collection
struct UseSortedList;
struct UseSkipList;

impl TestSortedCollection for UseSortedList {
    type CollectionType = SortedList<i32>;
}

impl TestSortedCollection for UseSkipList {
    type CollectionType = SkipList<i32>;
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_basic<T: TestSortedCollection>(#[case] _type: T) {
    let collection = EpochGuardedCollection::<i32, T::CollectionType>::default();
    test_basic_operations(&collection);
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_concurrent<T: TestSortedCollection>(#[case] _type: T) {
    test_concurrent_operations::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_concurrent_mixed<T: TestSortedCollection>(#[case] _type: T) {
    test_concurrent_mixed_operations::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_find_apply<T: TestSortedCollection>(#[case] _type: T) {
    let collection = EpochGuardedCollection::<i32, T::CollectionType>::default();
    test_find_and_apply(&collection);
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_sequential<T: TestSortedCollection>(#[case] _type: T) {
    test_sequential_operations::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_contention<T: TestSortedCollection>(#[case] _type: T) {
    test_high_contention::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_find_ref<T: TestSortedCollection>(#[case] _type: T) {
    test_find::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_remove_value<T: TestSortedCollection>(#[case] _type: T) {
    test_remove_returns_value::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_empty<T: TestSortedCollection>(#[case] _type: T) {
    test_is_empty::<EpochGuardedCollection<i32, T::CollectionType>>();
}

// ============================================================================
// insert_batch tests
// ============================================================================

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_basic<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_basic::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_empty<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_empty::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_with_duplicates<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_with_duplicates::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_with_existing<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_with_existing::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_from_unsorted<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_from_unsorted::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_preserves_order<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_preserves_order::<EpochGuardedCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_large<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_large::<EpochGuardedCollection<i32, T::CollectionType>>();
}

// ============================================================================
// Benchmark hang reproduction test
// This test exactly matches bench_concurrent_update from the benchmark
// ============================================================================

use starfish_core::data_structures::SafeSortedCollection;
use std::sync::Arc;
use std::thread;

/// Test that exactly matches the hanging benchmark pattern.
/// Uses EpochGuardedCollection<i64, SkipList<i64>> - the exact type from the benchmark.
#[test]
#[serial]
fn test_benchmark_hang_reproduction_skiplist() {
    let list: Arc<EpochGuardedCollection<i64, SkipList<i64>>> =
        Arc::new(EpochGuardedCollection::default());

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
    let list: Arc<EpochGuardedCollection<i64, SkipList<i64>>> =
        Arc::new(EpochGuardedCollection::default());

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
/// #TODO isolates the issue,
#[test]
#[serial]
fn test_benchmark_extended_run_skiplist() {
    //let list: Arc<EpochGuardedCollection<i64, SortedList<i64>>> =
    //Arc::new(EpochGuardedCollection::default());

    let list: Arc<EpochGuardedCollection<i64, SkipList<i64>>> =
        Arc::new(EpochGuardedCollection::default());
    //let list: Arc<DeferredCollection<i64, SkipList<i64>>> =
    //        Arc::new(DeferredCollection::default());

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
                for j in 0..key_range {
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

    let list: Arc<EpochGuardedCollection<i64, SkipList<i64>>> =
        Arc::new(EpochGuardedCollection::default());

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
#[case::skip_list(UseSkipList)]
fn stress_concurrent_update_with_timeout<T: TestSortedCollection>(#[case] _type: T) {
    use starfish_core::common_tests::sorted_collection_stress_tests::test_concurrent_update_with_timeout;

    test_concurrent_update_with_timeout::<EpochGuardedCollection<i32, T::CollectionType>>();
}
