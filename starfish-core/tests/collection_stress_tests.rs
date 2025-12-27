use rstest::rstest;
use starfish_core::DeferredGuard;
use starfish_core::common_tests::sorted_collection_stress_tests::*;
use starfish_core::data_structures::SkipList;
use starfish_core::data_structures::SortedCollection;
use starfish_core::data_structures::SortedList;

type DeferredSortedList = SortedList<i32, DeferredGuard>;
type DeferredSkipList = SkipList<i32, DeferredGuard>;
type DeferredSortedList64 = SortedList<i64, DeferredGuard>;
type DeferredSkipList64 = SkipList<i64, DeferredGuard>;

// ============================================================================
// i32 stress tests
// ============================================================================

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_find_during_modifications<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_find_during_modifications::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_memory_ordering<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_memory_ordering::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_concurrent_delete_same_value<
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
>(
    #[case] _collection: C,
) {
    test_concurrent_delete_same_value::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_linearizability<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_linearizability::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_progress_guarantee<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_progress_guarantee::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_extreme_contention_single_key<
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
>(
    #[case] _collection: C,
) {
    test_extreme_contention_single_key::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_concurrent_find_and_modify<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_concurrent_find_and_modify::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_high_contention_mixed<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_high_contention_mixed::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_aba_problem<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_aba_problem::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_extreme_contention_update_single_key<
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
>(
    #[case] _collection: C,
) {
    test_extreme_contention_update_single_key::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_focused_single_key_update<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_focused_single_key_update::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn stress_concurrent_update_with_timeout<
    C: SortedCollection<i32> + Default + Send + Sync + 'static,
>(
    #[case] _collection: C,
) {
    test_concurrent_update_with_timeout::<C>();
}

// ============================================================================
// i64 tests for benchmark pattern matching
// ============================================================================

#[rstest]
#[case::sorted_list(DeferredSortedList64::default())]
#[case::skip_list(DeferredSkipList64::default())]
fn stress_concurrent_update_benchmark_pattern<
    C: SortedCollection<i64> + Default + Send + Sync + 'static,
>(
    #[case] _collection: C,
) {
    test_concurrent_update_benchmark_pattern::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList64::default())]
#[case::skip_list(DeferredSkipList64::default())]
fn stress_concurrent_update_high_contention<
    C: SortedCollection<i64> + Default + Send + Sync + 'static,
>(
    #[case] _collection: C,
) {
    test_concurrent_update_high_contention::<C>();
}
