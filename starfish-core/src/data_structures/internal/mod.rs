//! Internal implementation details.
//!
//! These are pub(crate) and not intended for external use.

pub mod atomic_ptr;
pub mod marked_ptr;
#[allow(dead_code)]
pub mod marked_ptr_3bit;
pub mod sorted_collection;
pub mod unpublished_node;

pub(crate) use atomic_ptr::load_consume;
pub(crate) use atomic_ptr::prefetch_read;
pub(crate) use marked_ptr::MarkedPtr;
pub(crate) use unpublished_node::UnpublishedNode;
// These are made public for external wrappers like EpochGuardedCollection
pub use sorted_collection::CollectionNode;
pub use sorted_collection::NodePosition;
pub use sorted_collection::SortedCollection;
pub use sorted_collection::SortedCollectionInternal;
pub use sorted_collection::SortedCollectionIter;
