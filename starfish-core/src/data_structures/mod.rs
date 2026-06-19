//! Data structures for concurrent collections.
//!
//! # Organization
//!
//! - [`sorted`] - Lock-free sorted collections (SkipList, SortedList, Treap)
//! - [`hash`] - Hash-based collections (SplitOrderedHashMap, MapCollection trait)
//! - [`trie`] - Trie-based structures (SkipTrie, YFastTrie)
//! - [`internal`] - Internal implementation details (pub(crate))
//! - [`pair`] - Key-value `Pair<K, V>` wrapper (compares by key only)
//! - [`iterable_collection`] - Iteration traits for guarded references
//! - [`ordered_iterator`] - Marker traits for sorted-order iterators
//!
//! # Usage
//!
//! Collections are generic over guard type `G: Guard`:
//!
//! ```ignore
//! use starfish_core::{SortedList, DeferredGuard};
//!
//! let list: SortedList<i32, DeferredGuard> = SortedList::new();
//! list.insert(42);
//! ```

// Submodules
pub mod hash;
pub(crate) mod internal;
pub mod map_entry;
pub mod pair;
pub mod sorted;
pub mod trie;

// Top-level public modules
pub mod iterable_collection;
pub mod ordered_iterator;

// Re-exports for convenience
pub use hash::{MapCollection, MapNode, SplitOrderedHashMap};
pub use iterable_collection::{IterableCollection, IterableSortedCollection};
pub use map_entry::MapEntry;
pub use ordered_iterator::{Ordered, OrderedIterator, ordered_from_vec};
pub use pair::Pair;
pub use sorted::ListNodePosition;
pub use sorted::SkipList;
pub use sorted::SkipNodePosition;
pub use sorted::SortedList;
pub use sorted::Treap;
pub use trie::skip_trie::{KeyBits, SkipTrie};
pub use trie::y_fast_trie::YFastTrie;

// Re-export internal types
// MarkedPtr stays pub(crate) - truly internal implementation detail
// SortedCollection, CollectionNode are pub for external wrappers like EpochGuardedCollection
pub(crate) use internal::MarkedPtr;
// SortedCollectionInternal is pub inside the pub(crate) `internal` module,
// making it effectively crate-private. Not re-exported from the crate root.
pub(crate) use internal::SortedCollectionInternal;
pub use internal::{CollectionNode, NodePosition, SortedCollection, SortedCollectionIter};
