//! Internal implementation details.
//!
//! These are pub(crate) and not intended for external use.

pub mod marked_ptr;
pub mod marked_ptr_3bit;
pub mod sorted_collection;

pub(crate) use marked_ptr::MarkedPtr;
// These are made public for external wrappers like EpochGuardedCollection
pub use sorted_collection::CollectionNode;
pub use sorted_collection::NodePosition;
pub use sorted_collection::SortedCollection;
pub use sorted_collection::SortedCollectionIter;
