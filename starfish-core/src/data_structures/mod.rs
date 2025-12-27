//! Data structures for concurrent collections.
//!
//! # Organization
//!
//! - [`sorted`] - Lock-free sorted collections (SkipList, SortedList, Treap)
//! - [`hash`] - Hash-based collections
//! - [`trie`] - Trie-based structures
//! - [`internal`] - Internal implementation details (pub(crate))
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
pub mod sorted;
pub mod trie;

// Top-level public modules
pub mod iterable_collection;
pub mod ordered_iterator;

// Re-exports for convenience
pub use hash::{HashMapCollection, HashMapNode, SplitOrderedHashMap};
pub use iterable_collection::{IterableCollection, IterableSortedCollection};
pub use ordered_iterator::{Ordered, OrderedIterator, ordered_from_vec};
pub use sorted::ListNodePosition;
pub use sorted::SkipList;
pub use sorted::SkipNodePosition;
pub use sorted::SortedList;
pub use sorted::Treap;

// Re-export internal types
// MarkedPtr stays pub(crate) - truly internal implementation detail
// SortedCollection, CollectionNode are pub for external wrappers like EpochGuardedCollection
pub(crate) use internal::MarkedPtr;
pub use internal::{CollectionNode, NodePosition, SortedCollection, SortedCollectionIter};
