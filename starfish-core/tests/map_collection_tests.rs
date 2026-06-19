//! Tests for SkipList (value-CAS and node-replacement backends), SortedList,
//! SplitOrderedHashMap, and SkipTrie `MapCollection` implementations.
//!
//! Thin instantiation layer — generic test logic lives in
//! `src/common_tests/map_collection_core_tests.rs`.

use rstest::rstest;
use starfish_core::DeferredGuard;
use starfish_core::common_tests::map_collection_core_tests as map_tests;
use starfish_core::data_structures::hash::MapCollection;
use starfish_core::data_structures::hash::split_ordered_hash_map::SplitOrderedHashMap;
use starfish_core::data_structures::map_entry::MapEntry;
use starfish_core::data_structures::pair::Pair;
use starfish_core::data_structures::sorted::skip_list::SkipList;
use starfish_core::data_structures::sorted::sorted_list::SortedList;
use starfish_core::data_structures::trie::skip_trie::SkipTrie;

type SkipListMap<K, V> = SkipList<MapEntry<K, V>, DeferredGuard>;
type SkipListPairMap<K, V> = SkipList<Pair<K, V>, DeferredGuard>;
type SortedListMap<K, V> = SortedList<Pair<K, V>, DeferredGuard>;
type HashMap<K, V> = SplitOrderedHashMap<K, V, DeferredGuard>;
type SkipTrieMap<K, V> = SkipTrie<Pair<K, V>, DeferredGuard>;

// ============================================================================
// Tests that create their own collection internally.
// `_m` is constructed only for rstest type inference; the generic test
// functions build their own instances (often Arc-wrapped for threading).
// ============================================================================

#[rstest]
#[case::skip_list(SkipListMap::<usize, String>::default())]
#[case::skip_list_pair(SkipListPairMap::<usize, String>::default())]
#[case::sorted_list(SortedListMap::<usize, String>::default())]
#[case::hash_map(HashMap::<usize, String>::default())]
#[case::skip_trie(SkipTrieMap::<usize, String>::default())]
fn test_insert_contains_get<M: MapCollection<usize, String> + Default + Send + Sync>(
    #[case] _m: M,
) {
    map_tests::test_insert_contains_get::<M>();
}

#[rstest]
#[case::skip_list(SkipListMap::<usize, String>::default())]
#[case::skip_list_pair(SkipListPairMap::<usize, String>::default())]
#[case::sorted_list(SortedListMap::<usize, String>::default())]
#[case::hash_map(HashMap::<usize, String>::default())]
#[case::skip_trie(SkipTrieMap::<usize, String>::default())]
fn test_remove_returns_value<M: MapCollection<usize, String> + Default + Send + Sync>(
    #[case] _m: M,
) {
    map_tests::test_remove_returns_value::<M>();
}

#[rstest]
#[case::skip_list(SkipListMap::<usize, usize>::default())]
#[case::skip_list_pair(SkipListPairMap::<usize, usize>::default())]
#[case::sorted_list(SortedListMap::<usize, usize>::default())]
#[case::hash_map(HashMap::<usize, usize>::default())]
#[case::skip_trie(SkipTrieMap::<usize, usize>::default())]
fn test_len_accuracy<M: MapCollection<usize, usize> + Default + Send + Sync>(#[case] _m: M) {
    map_tests::test_len_accuracy::<M>();
}

#[rstest]
#[case::skip_list(SkipListMap::<usize, String>::default())]
#[case::skip_list_pair(SkipListPairMap::<usize, String>::default())]
#[case::sorted_list(SortedListMap::<usize, String>::default())]
#[case::hash_map(HashMap::<usize, String>::default())]
#[case::skip_trie(SkipTrieMap::<usize, String>::default())]
fn test_update_value_replacement<M: MapCollection<usize, String> + Default + Send + Sync>(
    #[case] _m: M,
) {
    map_tests::test_update_value_replacement::<M>();
}

#[rstest]
#[case::skip_list(SkipListMap::<usize, usize>::default())]
#[case::skip_list_pair(SkipListPairMap::<usize, usize>::default())]
#[case::sorted_list(SortedListMap::<usize, usize>::default())]
#[case::hash_map(HashMap::<usize, usize>::default())]
#[case::skip_trie(SkipTrieMap::<usize, usize>::default())]
fn test_find_and_apply<M: MapCollection<usize, usize> + Default + Send + Sync>(#[case] _m: M) {
    map_tests::test_find_and_apply::<M>();
}

#[rstest]
#[case::skip_list(SkipListMap::<usize, usize>::default())]
#[case::skip_list_pair(SkipListPairMap::<usize, usize>::default())]
#[case::sorted_list(SortedListMap::<usize, usize>::default())]
#[case::hash_map(HashMap::<usize, usize>::default())]
#[case::skip_trie(SkipTrieMap::<usize, usize>::default())]
fn test_concurrent_insert_get_remove<
    M: MapCollection<usize, usize> + Default + Send + Sync + 'static,
>(
    #[case] _m: M,
) {
    map_tests::test_concurrent_insert_get_remove::<M>();
}

#[rstest]
#[case::skip_list(SkipListMap::<usize, usize>::default())]
#[case::skip_list_pair(SkipListPairMap::<usize, usize>::default())]
#[case::sorted_list(SortedListMap::<usize, usize>::default())]
#[case::hash_map(HashMap::<usize, usize>::default())]
#[case::skip_trie(SkipTrieMap::<usize, usize>::default())]
fn test_concurrent_update<M: MapCollection<usize, usize> + Default + Send + Sync + 'static>(
    #[case] _m: M,
) {
    map_tests::test_concurrent_update::<M>();
}

// ============================================================================
// Signed-key tests — exercises KeyBits sign-bit flip through MapCollection API
// ============================================================================

#[test]
fn test_skip_trie_signed_keys() {
    let map = SkipTrieMap::<i32, String>::default();

    // Insert negative, zero, and positive keys
    assert!(map.insert(-10, "neg10".into()));
    assert!(map.insert(-1, "neg1".into()));
    assert!(map.insert(0, "zero".into()));
    assert!(map.insert(1, "pos1".into()));
    assert!(map.insert(i32::MIN, "min".into()));
    assert!(map.insert(i32::MAX, "max".into()));

    // Find all
    assert_eq!(map.get(&-10), Some("neg10".into()));
    assert_eq!(map.get(&-1), Some("neg1".into()));
    assert_eq!(map.get(&0), Some("zero".into()));
    assert_eq!(map.get(&1), Some("pos1".into()));
    assert_eq!(map.get(&i32::MIN), Some("min".into()));
    assert_eq!(map.get(&i32::MAX), Some("max".into()));

    // Duplicate insert for negative key
    assert!(!map.insert(-1, "dup_neg1".into()));
    assert_eq!(map.get(&-1), Some("neg1".into()));

    // Update across sign boundary
    assert!(map.update(-1, "updated_neg1".into()));
    assert_eq!(map.get(&-1), Some("updated_neg1".into()));
    assert!(map.update(0, "updated_zero".into()));
    assert_eq!(map.get(&0), Some("updated_zero".into()));

    // Remove and verify
    assert_eq!(map.remove(&-10), Some("neg10".into()));
    assert!(!map.contains(&-10));
    assert_eq!(map.remove(&i32::MIN), Some("min".into()));
    assert!(!map.contains(&i32::MIN));

    assert_eq!(map.len(), 4);
}
