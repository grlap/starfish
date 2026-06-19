//! Shared test suites and benchmark utilities for collection implementations.
//!
//! Contains core correctness tests and stress tests that can be reused
//! across different collection backends (SkipList, SortedList, etc.),
//! plus shared benchmark helpers (`bench_utils`).

#[cfg(any(test, feature = "sorted_collection_core_tests"))]
pub mod sorted_collection_core_tests;

#[cfg(any(test, feature = "sorted_collection_core_tests"))]
pub mod sorted_collection_stress_tests;

#[cfg(any(test, feature = "map_collection_core_tests"))]
pub mod map_collection_core_tests;

#[cfg(any(test, feature = "bench_utils"))]
pub mod bench_utils;
