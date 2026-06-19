//! Crossbeam-epoch memory reclamation for starfish collections.
//!
//! Provides `EpochGuard`, an implementation of the `Guard` trait that uses
//! crossbeam-epoch for safe deferred destruction, and `EpochRef` for holding
//! references protected by an epoch guard.
//!
//! # Usage
//!
//! ```ignore
//! use starfish_core::data_structures::{SortedList, SortedCollection};
//! use starfish_crossbeam::EpochGuard;
//!
//! let list: SortedList<i32, EpochGuard> = SortedList::new();
//! list.insert(42);
//!
//! if let Some(val) = list.find(&42) {
//!     println!("Found: {}", *val);
//! }
//! ```

pub mod epoch_guard;

// Export the Guard implementation
pub use epoch_guard::{EpochGuard, EpochRef};
