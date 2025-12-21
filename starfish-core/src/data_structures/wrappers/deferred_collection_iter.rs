//! Iterator implementation for DeferredCollection.

use std::marker::PhantomData;

use super::deferred_collection::{DeferredCollection, DeferredGuardedRef};
use crate::data_structures::{CollectionNode, SortedCollection};

// ============================================================================
// DeferredCollectionIter - Iterator implementation for DeferredCollection
// ============================================================================

/// Iterator over DeferredCollection elements.
pub struct DeferredCollectionIter<'a, T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    collection: &'a DeferredCollection<T, C>,
    current_node: Option<*mut C::Node>,
    _phantom: PhantomData<T>,
}

impl<'a, T, C> DeferredCollectionIter<'a, T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    pub(crate) fn new(collection: &'a DeferredCollection<T, C>) -> Self {
        let first = collection.inner().first_node_internal();
        Self {
            collection,
            current_node: first,
            _phantom: PhantomData,
        }
    }

    pub(crate) fn from_node(
        collection: &'a DeferredCollection<T, C>,
        node: Option<*mut C::Node>,
    ) -> Self {
        Self {
            collection,
            current_node: node,
            _phantom: PhantomData,
        }
    }
}

impl<'a, T, C> Iterator for DeferredCollectionIter<'a, T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord + Clone,
{
    type Item = DeferredGuardedRef<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.current_node?;

        // Get next node before returning current
        self.current_node = self.collection.inner().next_node_internal(node);

        // Get reference to current node's data
        unsafe {
            let data = (*node).key();
            Some(DeferredGuardedRef::new(data))
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use crate::data_structures::{DeferredCollection, SafeSortedCollection, SkipList, SortedList};

    #[test]
    fn test_iter_basic() {
        let collection: DeferredCollection<i32, SortedList<i32>> = DeferredCollection::default();

        collection.insert(5);
        collection.insert(3);
        collection.insert(7);
        collection.insert(1);

        let values: Vec<i32> = collection.iter().map(|r| *r).collect();
        assert_eq!(values, vec![1, 3, 5, 7]);
    }

    #[test]
    fn test_iter_empty() {
        let collection: DeferredCollection<i32, SortedList<i32>> = DeferredCollection::default();

        let values: Vec<i32> = collection.iter().map(|r| *r).collect();
        assert!(values.is_empty());
    }

    #[test]
    fn test_to_vec() {
        let collection: DeferredCollection<i32, SortedList<i32>> = DeferredCollection::default();

        for i in [10, 5, 15, 3, 7] {
            collection.insert(i);
        }

        let values = collection.to_vec();
        assert_eq!(values, vec![3, 5, 7, 10, 15]);
    }

    #[test]
    fn test_len() {
        let collection: DeferredCollection<i32, SortedList<i32>> = DeferredCollection::default();

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
        let collection: DeferredCollection<i32, SortedList<i32>> = DeferredCollection::default();

        for i in 1..=5 {
            collection.insert(i);
        }

        collection.delete(&3);

        let values: Vec<i32> = collection.iter().map(|r| *r).collect();
        assert_eq!(values, vec![1, 2, 4, 5]);
    }

    #[test]
    fn test_iter_from() {
        let collection: DeferredCollection<i32, SortedList<i32>> = DeferredCollection::default();

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
        let collection: DeferredCollection<i32, SkipList<i32>> = DeferredCollection::default();

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
