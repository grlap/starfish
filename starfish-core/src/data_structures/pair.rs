//! Key-value pair wrapper for sorted collections.
//!
//! Provides `Pair<K, V>` which implements `Ord`/`Eq` by key only, carrying `V` inline.
//! Used as the map payload for `SortedList` and `SkipTrie`.
//!
//! NOTE: `SkipList` maps use [`MapEntry`](super::map_entry::MapEntry) (value behind an
//! `AtomicPtr`, value-CAS) rather than `Pair` node-replacement — NOT because node-replacing a
//! `Pair` is unsafe (single-level `SortedList` and multi-level `SkipTrie` node-replacement are
//! both validated epoch-safe: the old node is unlinked before retire; see
//! `starfish-crossbeam/tests/{sorted_list,skip_trie}_map_epoch.rs`; their iterators skip a node's
//! same-key UPDATE replacement, so iteration is duplicate-free too), but because the SkipList
//! *helping* variant could not guarantee unlink-before-retire, and value-CAS additionally makes
//! UPDATE a single CAS and needs no `UPDATE_MARK`. See `MapEntry` for the value-CAS UPDATE design.

use std::borrow::Borrow;

/// A key-value pair that compares and orders only by key.
///
/// Lets `SortedList` / `SkipTrie` act as a key-value map: the collection orders by `K`,
/// and `V` is carried inline as payload. (For `SkipList` maps use
/// [`MapEntry`](super::map_entry::MapEntry).)
///
/// # Example
/// ```ignore
/// use starfish_core::data_structures::{Pair, SortedCollection};
/// use starfish_core::data_structures::sorted::sorted_list::SortedList;
///
/// let map: SortedList<Pair<String, i64>, DeferredGuard> = SortedList::new();
/// map.insert(Pair::new("alice".into(), 100));
/// ```
#[derive(Debug, Clone)]
pub struct Pair<K, V> {
    pub key: K,
    pub value: V,
}

impl<K, V> Pair<K, V> {
    #[inline]
    pub fn new(key: K, value: V) -> Self {
        Pair { key, value }
    }
}

impl<K: PartialEq, V> PartialEq for Pair<K, V> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K: Eq, V> Eq for Pair<K, V> {}

impl<K: PartialOrd, V> PartialOrd for Pair<K, V> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.key.partial_cmp(&other.key)
    }
}

impl<K: Ord, V> Ord for Pair<K, V> {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

impl<K: Default, V: Default> Default for Pair<K, V> {
    fn default() -> Self {
        Pair {
            key: K::default(),
            value: V::default(),
        }
    }
}

impl<K, V> Borrow<K> for Pair<K, V> {
    #[inline]
    fn borrow(&self) -> &K {
        &self.key
    }
}
