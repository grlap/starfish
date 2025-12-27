//! Lock-free sorted collection implementations.
//!
//! Collections are parameterized by a guard type `G: Guard` that determines
//! the memory reclamation strategy:
//!
//! - `DeferredGuard`: Testing - defers destruction until guard drops
//! - `EpochGuard`: Production - epoch-based reclamation (crossbeam-epoch)

pub mod skip_list;
pub mod sorted_list;
pub mod treap;

pub use skip_list::{SkipList, SkipNodePosition};
pub use sorted_list::{ListNodePosition, SortedList};
pub use treap::Treap;
