//! Iteration traits for collections.
//!
//! This module provides base traits for safe iteration over collections.
//! The design supports both sorted collections (with range queries) and
//! unordered collections like HashMap.
//!
//! Note: Currently, SafeSortedCollection includes iteration methods directly.
//! These traits are kept for future HashMap support where we want iteration
//! without requiring Ord.

use std::ops::Deref;

// ============================================================================
// IterableCollection - Base trait for any iterable collection
// ============================================================================

/// Trait for collections that support safe iteration.
///
/// This is generic enough to support SortedCollection, HashMap, and other
/// collection types. It's kept separate from SafeSortedCollection for future
/// use with unordered collections.
///
pub trait IterableCollection<T> {
    /// The type of guarded reference returned during iteration.
    /// This ensures memory safety for lock-free structures.
    type GuardedRef<'a>: Deref<Target = T>
    where
        Self: 'a;

    /// Iterator type that yields guarded references.
    type Iter<'a>: Iterator<Item = Self::GuardedRef<'a>>
    where
        Self: 'a;

    /// Returns an iterator over all elements in the collection.
    fn iter(&self) -> Self::Iter<'_>;

    /// Collects all elements into a Vec (convenience method).
    fn to_vec(&self) -> Vec<T>
    where
        T: Clone;

    /// Returns the number of elements in the collection.
    fn len(&self) -> usize;

    /// Returns true if the collection is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ============================================================================
// IterableSortedCollection - Extended trait for sorted collections
// ============================================================================

/// Extended trait for sorted collections that support range iteration.
/// HashMap won't implement this, but SortedCollection types will.
///
pub trait IterableSortedCollection<T: Ord>: IterableCollection<T> {
    /// Returns an iterator starting from the given key (inclusive).
    fn iter_from(&self, start_key: &T) -> Self::Iter<'_>;
}
