//! Ordered iterator traits and wrappers for type-safe batch operations.
//!
//! This module provides marker traits and wrappers to indicate that an iterator
//! yields elements in sorted order. This enables optimized batch insertion
//! algorithms that can skip internal sorting when the input is already ordered.
//!
//! # Example
//!
//! ```ignore
//! use starfish_core::data_structures::ordered_iterator::{Ordered, OrderedIterator};
//!
//! // Wrap an iterator that you know is ordered
//! let ordered_data = vec![1, 2, 3, 4, 5];
//! let ordered_iter = Ordered::new(ordered_data.into_iter());
//!
//! // Use with batch insert (when implemented)
//! // collection.insert_batch(ordered_iter);
//! ```

use std::iter::FusedIterator;

// ============================================================================
// OrderedIterator - Marker trait for ordered iterators
// ============================================================================

/// Marker trait for iterators that yield elements in ascending order.
///
/// This trait is used to enable optimized batch operations that can skip
/// internal sorting when the input is already ordered.
///
/// # Contract
///
/// Implementors must guarantee that elements are yielded in ascending order
/// according to `Ord`. That is, for any two consecutive elements `a` and `b`
/// where `a` is yielded before `b`, it must hold that `a <= b`.
///
/// # Safety
///
/// This is not an unsafe trait, but violating the ordered contract may cause
/// incorrect behavior in algorithms that rely on it (e.g., duplicate detection,
/// splice-based insertion).
pub trait OrderedIterator: Iterator
where
    Self::Item: Ord,
{
}

// ============================================================================
// Ordered - Wrapper with runtime verification
// ============================================================================

/// A wrapper that verifies ascending order at runtime.
///
/// Each call to `next()` checks that the current element is >= the previous
/// element. This catches programming errors early and panics when the
/// ordered contract is violated.
///
/// # Panics
///
/// Panics if elements are not in ascending order.
///
/// # Example
///
/// ```ignore
/// use starfish_core::data_structures::ordered_iterator::Ordered;
///
/// let data = vec![1, 2, 3, 4, 5];
/// let ordered = Ordered::new(data.into_iter());
///
/// // Will panic if any element is less than the previous
/// for item in ordered {
///     println!("{}", item);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Ordered<I: Iterator>
where
    I::Item: Ord + Clone,
{
    inner: I,
    last: Option<I::Item>,
}

impl<I: Iterator> Ordered<I>
where
    I::Item: Ord + Clone,
{
    /// Create a new ordered iterator wrapper.
    ///
    /// Each call to `next()` will verify that elements are in ascending order.
    ///
    /// # Panics
    ///
    /// The returned iterator will panic if elements are not in ascending order.
    #[inline]
    pub fn new(iter: I) -> Self {
        Ordered {
            inner: iter,
            last: None,
        }
    }

    /// Unwrap and return the inner iterator.
    ///
    /// Note: This discards the "last seen" state, so order verification
    /// will reset if wrapped again.
    #[inline]
    pub fn into_inner(self) -> I {
        self.inner
    }
}

impl<I: Iterator> Iterator for Ordered<I>
where
    I::Item: Ord + Clone,
{
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next()?;

        if let Some(ref last) = self.last {
            assert!(
                last <= &item,
                "OrderedIterator contract violated: previous element > current element"
            );
        }

        self.last = Some(item.clone());
        Some(item)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<I: Iterator> OrderedIterator for Ordered<I> where I::Item: Ord + Clone {}

impl<I: ExactSizeIterator> ExactSizeIterator for Ordered<I>
where
    I::Item: Ord + Clone,
{
    #[inline]
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<I: FusedIterator> FusedIterator for Ordered<I> where I::Item: Ord + Clone {}

// ============================================================================
// Helper functions
// ============================================================================

/// Create an ordered iterator from a Vec, sorting it first.
///
/// This is a convenience function that sorts the input and wraps it
/// in an `Ordered` wrapper with verification.
#[inline]
pub fn ordered_from_vec<T: Ord + Clone>(mut vec: Vec<T>) -> Ordered<std::vec::IntoIter<T>> {
    vec.sort();
    Ordered::new(vec.into_iter())
}

/// Create an ordered iterator from a Vec using unstable sort.
///
/// Like `ordered_from_vec`, but uses unstable sort which may be faster
/// but doesn't preserve order of equal elements.
#[inline]
pub fn ordered_from_vec_unstable<T: Ord + Clone>(
    mut vec: Vec<T>,
) -> Ordered<std::vec::IntoIter<T>> {
    vec.sort_unstable();
    Ordered::new(vec.into_iter())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ordered_valid() {
        let data = vec![1, 2, 3, 4, 5];
        let ordered = Ordered::new(data.into_iter());

        let collected: Vec<_> = ordered.collect();
        assert_eq!(collected, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_ordered_with_duplicates() {
        let data = vec![1, 2, 2, 3, 3, 3, 4, 5];
        let ordered = Ordered::new(data.into_iter());

        let collected: Vec<_> = ordered.collect();
        assert_eq!(collected, vec![1, 2, 2, 3, 3, 3, 4, 5]);
    }

    #[test]
    #[should_panic(expected = "OrderedIterator contract violated")]
    fn test_ordered_invalid() {
        let data = vec![1, 3, 2, 4, 5]; // Not ordered!
        let ordered = Ordered::new(data.into_iter());

        // Should panic when we hit 2 after 3
        let _: Vec<_> = ordered.collect();
    }

    #[test]
    fn test_ordered_size_hint() {
        let data = vec![1, 2, 3, 4, 5];
        let ordered = Ordered::new(data.into_iter());

        assert_eq!(ordered.size_hint(), (5, Some(5)));
    }

    #[test]
    fn test_ordered_exact_size() {
        let data = vec![1, 2, 3, 4, 5];
        let ordered = Ordered::new(data.into_iter());

        assert_eq!(ordered.len(), 5);
    }

    #[test]
    fn test_ordered_from_vec() {
        let data = vec![5, 3, 1, 4, 2];
        let ordered = ordered_from_vec(data);

        let collected: Vec<_> = ordered.collect();
        assert_eq!(collected, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_ordered_from_vec_unstable() {
        let data = vec![5, 3, 1, 4, 2];
        let ordered = ordered_from_vec_unstable(data);

        let collected: Vec<_> = ordered.collect();
        assert_eq!(collected, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_ordered_into_inner() {
        let data = vec![1, 2, 3];
        let ordered = Ordered::new(data.into_iter());
        let inner = ordered.into_inner();

        let collected: Vec<_> = inner.collect();
        assert_eq!(collected, vec![1, 2, 3]);
    }

    #[test]
    fn test_ordered_empty() {
        let data: Vec<i32> = vec![];
        let ordered = Ordered::new(data.into_iter());

        let collected: Vec<_> = ordered.collect();
        assert!(collected.is_empty());
    }

    #[test]
    fn test_ordered_single_element() {
        let data = vec![42];
        let ordered = Ordered::new(data.into_iter());

        let collected: Vec<_> = ordered.collect();
        assert_eq!(collected, vec![42]);
    }
}
