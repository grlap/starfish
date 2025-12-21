use rstest::rstest;
use serial_test::serial;
use starfish_core::common_tests::sorted_collection_stress_tests::*;
use starfish_core::data_structures::{DeferredCollection, SkipList, SortedCollection, SortedList};

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
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_find_during_modifications<T: TestSortedCollection>(#[case] _type: T) {
    test_find_during_modifications::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_memory_ordering<T: TestSortedCollection>(#[case] _type: T) {
    test_memory_ordering::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_concurrent_delete_same_value<T: TestSortedCollection>(#[case] _type: T) {
    test_concurrent_delete_same_value::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_linearizability<T: TestSortedCollection>(#[case] _type: T) {
    test_linearizability::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_progress_guarantee<T: TestSortedCollection>(#[case] _type: T) {
    test_progress_guarantee::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_extreme_contention_single_key<T: TestSortedCollection>(#[case] _type: T) {
    test_extreme_contention_single_key::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_concurrent_find_and_modify<T: TestSortedCollection>(#[case] _type: T) {
    test_concurrent_find_and_modify::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_high_contention_mixed<T: TestSortedCollection>(#[case] _type: T) {
    test_high_contention_mixed::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_aba_problem<T: TestSortedCollection>(#[case] _type: T) {
    test_aba_problem::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_extreme_contention_update_single_key<T: TestSortedCollection>(#[case] _type: T) {
    test_extreme_contention_update_single_key::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_focused_single_key_update<T: TestSortedCollection>(#[case] _type: T) {
    test_focused_single_key_update::<DeferredCollection<i32, T::CollectionType>>();
}

// ============================================================================
// i64 tests for benchmark pattern matching
// These tests match the exact patterns from sorted_collection_benchmark.rs
// ============================================================================

// Trait for i64 type-level parametrization
trait TestSortedCollection64 {
    type CollectionType: SortedCollection<i64> + Default + Send + Sync + 'static;
}

struct UseSortedList64;
struct UseSkipList64;

impl TestSortedCollection64 for UseSortedList64 {
    type CollectionType = SortedList<i64>;
}

impl TestSortedCollection64 for UseSkipList64 {
    type CollectionType = SkipList<i64>;
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList64)]
#[case::skip_list(UseSkipList64)]
fn stress_concurrent_update_benchmark_pattern<T: TestSortedCollection64>(#[case] _type: T) {
    test_concurrent_update_benchmark_pattern::<DeferredCollection<i64, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList64)]
#[case::skip_list(UseSkipList64)]
fn stress_concurrent_update_high_contention<T: TestSortedCollection64>(#[case] _type: T) {
    test_concurrent_update_high_contention::<DeferredCollection<i64, T::CollectionType>>();
}

#[rstest]
#[serial(stress_tests)]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn stress_concurrent_update_with_timeout<T: TestSortedCollection>(#[case] _type: T) {
    test_concurrent_update_with_timeout::<DeferredCollection<i32, T::CollectionType>>();
}
