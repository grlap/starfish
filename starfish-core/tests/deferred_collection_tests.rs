use rstest::rstest;
use starfish_core::common_tests::sorted_collection_core_tests::*;
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
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_basic<T: TestSortedCollection>(#[case] _type: T) {
    let collection = DeferredCollection::<i32, T::CollectionType>::default();
    test_basic_operations(&collection);
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_concurrent<T: TestSortedCollection>(#[case] _type: T) {
    test_concurrent_operations::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_concurrent_mixed<T: TestSortedCollection>(#[case] _type: T) {
    test_concurrent_mixed_operations::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_find_apply<T: TestSortedCollection>(#[case] _type: T) {
    let collection = DeferredCollection::<i32, T::CollectionType>::default();
    test_find_and_apply(&collection);
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_remove_value<T: TestSortedCollection>(#[case] _type: T) {
    test_remove_returns_value::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_find_ref<T: TestSortedCollection>(#[case] _type: T) {
    test_find::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_sequential<T: TestSortedCollection>(#[case] _type: T) {
    test_sequential_operations::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_contention<T: TestSortedCollection>(#[case] _type: T) {
    test_high_contention::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_empty<T: TestSortedCollection>(#[case] _type: T) {
    test_is_empty::<DeferredCollection<i32, T::CollectionType>>();
}

// ============================================================================
// insert_batch tests
// ============================================================================

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_basic<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_basic::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_empty<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_empty::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_with_duplicates<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_with_duplicates::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_with_existing<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_with_existing::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_from_unsorted<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_from_unsorted::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_preserves_order<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_preserves_order::<DeferredCollection<i32, T::CollectionType>>();
}

#[rstest]
#[case::sorted_list(UseSortedList)]
#[case::skip_list(UseSkipList)]
fn test_batch_large<T: TestSortedCollection>(#[case] _type: T) {
    test_insert_batch_large::<DeferredCollection<i32, T::CollectionType>>();
}
