//! Iterator implementation for EpochGuardedCollection.

use std::marker::PhantomData;

use crossbeam_epoch::{self as epoch, Guard};

use starfish_core::data_structures::{CollectionNode, SortedCollection};

use crate::epoch_guarded_sorted_collection::EpochGuardedCollection;
use crate::guarded_ref::GuardedRef;

// ============================================================================
// EpochGuardedCollectionIter - Iterator implementation for EpochGuardedCollection
// ============================================================================

/// Iterator over EpochGuardedCollection elements.
///
/// This iterator holds an epoch guard for the entire duration of iteration,
/// ensuring memory safety. The guard is only released when the iterator is
/// dropped.
pub struct EpochGuardedCollectionIter<'a, T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    _guard: Guard,
    collection: &'a EpochGuardedCollection<T, C>,
    current_node: Option<*mut C::Node>,
    _phantom: PhantomData<T>,
}

impl<'a, T, C> EpochGuardedCollectionIter<'a, T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    pub(crate) fn new(collection: &'a EpochGuardedCollection<T, C>) -> Self {
        let guard = epoch::pin();
        let first = collection.inner().first_node_internal();
        Self {
            _guard: guard,
            collection,
            current_node: first,
            _phantom: PhantomData,
        }
    }

    pub(crate) fn from_node(
        collection: &'a EpochGuardedCollection<T, C>,
        node: Option<*mut C::Node>,
    ) -> Self {
        let guard = epoch::pin();
        Self {
            _guard: guard,
            collection,
            current_node: node,
            _phantom: PhantomData,
        }
    }
}

impl<'a, T, C> Iterator for EpochGuardedCollectionIter<'a, T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord + Clone,
{
    type Item = GuardedRef<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.current_node?;

        // Get next node before returning current
        self.current_node = self.collection.inner().next_node_internal(node);

        // Get reference to current node's data
        // Safety: The epoch guard protects this access
        unsafe {
            let data = (*node).key();
            // We need to create a new guard for each GuardedRef because
            // GuardedRef owns its guard. This is safe because the iterator's
            // guard keeps the memory valid.
            let new_guard = epoch::pin();
            Some(GuardedRef::new(new_guard, data))
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use starfish_core::data_structures::{SafeSortedCollection, SkipList, SortedList};

    use crate::epoch_guarded_sorted_collection::EpochGuardedCollection;

    #[test]
    fn test_iter_basic() {
        let collection: EpochGuardedCollection<i32, SortedList<i32>> =
            EpochGuardedCollection::default();

        collection.insert(5);
        collection.insert(3);
        collection.insert(7);
        collection.insert(1);

        let values: Vec<i32> = collection.iter().map(|r| *r).collect();
        assert_eq!(values, vec![1, 3, 5, 7]);
    }

    #[test]
    fn test_iter_empty() {
        let collection: EpochGuardedCollection<i32, SortedList<i32>> =
            EpochGuardedCollection::default();

        let values: Vec<i32> = collection.iter().map(|r| *r).collect();
        assert!(values.is_empty());
    }

    #[test]
    fn test_to_vec() {
        let collection: EpochGuardedCollection<i32, SortedList<i32>> =
            EpochGuardedCollection::default();

        for i in [10, 5, 15, 3, 7] {
            collection.insert(i);
        }

        let values = collection.to_vec();
        assert_eq!(values, vec![3, 5, 7, 10, 15]);
    }

    #[test]
    fn test_len() {
        let collection: EpochGuardedCollection<i32, SortedList<i32>> =
            EpochGuardedCollection::default();

        assert_eq!(collection.len(), 0);
        assert!(collection.is_empty());

        collection.insert(1);
        collection.insert(2);
        collection.insert(3);

        assert_eq!(collection.len(), 3);
        assert!(!collection.is_empty());
    }

    #[test]
    fn test_iter_after_delete() {
        let collection: EpochGuardedCollection<i32, SortedList<i32>> =
            EpochGuardedCollection::default();

        for i in 1..=5 {
            collection.insert(i);
        }

        collection.delete(&3);

        let values: Vec<i32> = collection.iter().map(|r| *r).collect();
        assert_eq!(values, vec![1, 2, 4, 5]);
    }

    #[test]
    fn test_iter_from() {
        let collection: EpochGuardedCollection<i32, SortedList<i32>> =
            EpochGuardedCollection::default();

        for i in [10, 20, 30, 40, 50] {
            collection.insert(i);
        }

        // Start from 25 - should get 30, 40, 50
        let values: Vec<i32> = collection.iter_from(&25).map(|r| *r).collect();
        assert_eq!(values, vec![30, 40, 50]);

        // Start from 30 - should get 30, 40, 50
        let values: Vec<i32> = collection.iter_from(&30).map(|r| *r).collect();
        assert_eq!(values, vec![30, 40, 50]);

        // Start from 0 - should get all
        let values: Vec<i32> = collection.iter_from(&0).map(|r| *r).collect();
        assert_eq!(values, vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn test_iter_skiplist() {
        let collection: EpochGuardedCollection<i32, SkipList<i32>> =
            EpochGuardedCollection::default();

        collection.insert(5);
        collection.insert(3);
        collection.insert(7);
        collection.insert(1);
        collection.insert(9);

        let values: Vec<i32> = collection.iter().map(|r| *r).collect();
        assert_eq!(values, vec![1, 3, 5, 7, 9]);

        // Test to_vec
        let vec = collection.to_vec();
        assert_eq!(vec, vec![1, 3, 5, 7, 9]);

        // Test len
        assert_eq!(collection.len(), 5);

        // Test iter_from
        let from_5: Vec<i32> = collection.iter_from(&5).map(|r| *r).collect();
        assert_eq!(from_5, vec![5, 7, 9]);
    }
}
