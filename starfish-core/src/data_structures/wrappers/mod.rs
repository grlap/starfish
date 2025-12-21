//! Collection wrappers for safe memory reclamation.

pub mod deferred_collection;
pub mod deferred_collection_iter;
pub mod deferred_hash_map;

pub use deferred_collection::DeferredCollection;
pub use deferred_hash_map::DeferredHashMap;
