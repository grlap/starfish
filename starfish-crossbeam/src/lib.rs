//! Crossbeam-based implementations for starfish collections.
//!
//! This crate provides `EpochGuard`, an implementation of the `Guard` trait
//! using crossbeam-epoch for memory reclamation.
//!
//! # Usage
//!
//! ```ignore
//! use starfish_core::SortedList;
//! use starfish_crossbeam::EpochGuard;
//!
//! let list: SortedList<i32, EpochGuard> = SortedList::new();
//! list.insert(42);
//! ```

pub mod epoch_guard;

// Export the Guard implementation
pub use epoch_guard::{EpochGuard, EpochRef};
