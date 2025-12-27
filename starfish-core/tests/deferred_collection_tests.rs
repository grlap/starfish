use rstest::rstest;
use starfish_core::DeferredGuard;
use starfish_core::common_tests::sorted_collection_core_tests::*;
use starfish_core::data_structures::SkipList;
use starfish_core::data_structures::SortedCollection;
use starfish_core::data_structures::SortedList;

type DeferredSortedList = SortedList<i32, DeferredGuard>;
type DeferredSkipList = SkipList<i32, DeferredGuard>;

// ============================================================================
// Tests that take a collection reference
// ============================================================================

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_basic<C: SortedCollection<i32>>(#[case] collection: C) {
    test_basic_operations(&collection);
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_find_apply<C: SortedCollection<i32>>(#[case] collection: C) {
    test_find_and_apply(&collection);
}

// ============================================================================
// Tests that create their own collection (for threading)
// ============================================================================

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_concurrent<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_concurrent_operations::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_concurrent_mixed<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_concurrent_mixed_operations::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_contention<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_high_contention::<C>();
}

// ============================================================================
// Tests that use Default (can use rstest with type)
// ============================================================================

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_remove_value<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_remove_returns_value::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_find_ref<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_find::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_sequential<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_sequential_operations::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_empty<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_is_empty::<C>();
}

// ============================================================================
// Insert batch tests
// ============================================================================

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_batch_basic<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_basic::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_batch_empty<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_empty::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_batch_with_duplicates<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_with_duplicates::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_batch_with_existing<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_with_existing::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_batch_from_unsorted<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_from_unsorted::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_batch_preserves_order<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_preserves_order::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_batch_large<C: SortedCollection<i32> + Default>(#[case] _collection: C) {
    test_insert_batch_large::<C>();
}

// ============================================================================
// New tests
// ============================================================================

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_update<C: SortedCollection<i32> + Default>(#[case] collection: C) {
    test_update_operations(&collection);
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_concurrent_update<C: SortedCollection<i32> + Default + Send + Sync + 'static>(
    #[case] _collection: C,
) {
    test_concurrent_update_operations::<C>();
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_len<C: SortedCollection<i32> + Default>(#[case] collection: C) {
    test_len_operations(&collection);
}

#[rstest]
#[case::sorted_list(DeferredSortedList::default())]
#[case::skip_list(DeferredSkipList::default())]
fn test_iter<C: SortedCollection<i32> + Default>(#[case] collection: C) {
    test_iter_operations(&collection);
}
