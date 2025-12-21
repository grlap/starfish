use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::Mutex;

use super::deferred_collection_iter::DeferredCollectionIter;
use crate::data_structures::{
    CollectionNode, NodePosition, OrderedIterator, SafeSortedCollection, SortedCollection,
};

/// Collection wrapper that defers node destruction until drop.
/// Compatible with backlinks - marked nodes stay valid until cleanup.
///
/// Use for the testing only.
///
pub struct DeferredCollection<T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    inner: C,
    deferred_nodes: Mutex<Vec<*mut C::Node>>,
    _phantom: PhantomData<T>,
}

impl<T, C> DeferredCollection<T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    pub fn new(collection: C) -> Self {
        DeferredCollection {
            inner: collection,
            deferred_nodes: Mutex::new(Vec::new()),
            _phantom: PhantomData,
        }
    }

    /// Internal access to inner collection for iterator implementation.
    pub(crate) fn inner(&self) -> &C {
        &self.inner
    }
}

pub struct DeferredGuardedRef<'a, T> {
    data: &'a T,
}

impl<'a, T> DeferredGuardedRef<'a, T> {
    /// Creates a new guarded reference from a data reference.
    pub fn new(data: &'a T) -> Self {
        DeferredGuardedRef { data }
    }
}

impl<'a, T> Deref for DeferredGuardedRef<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl<T, C> SafeSortedCollection<T> for DeferredCollection<T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord + Clone,
{
    type GuardedRef<'a>
        = DeferredGuardedRef<'a, T>
    where
        Self: 'a,
        T: 'a;

    type Iter<'a>
        = DeferredCollectionIter<'a, T, C>
    where
        Self: 'a;

    fn insert(&self, key: T) -> bool {
        self.inner.insert_from_internal(key, None).is_some()
    }

    fn delete(&self, key: &T) -> bool {
        if let Some(pos) = self.inner.remove_from_internal(None, key) {
            // Remove the node from the collection and defer delete.
            //
            self.deferred_nodes.lock().unwrap().push(pos.node_ptr());
            true
        } else {
            false
        }
    }

    fn remove(&self, key: &T) -> Option<T> {
        // Remove the node from the collection.
        //
        let pos = self.inner.remove_from_internal(None, key)?;

        // Clone the value - we can't take it because other threads may still be reading.
        // The node keeps its value until deallocation.
        //
        let data = unsafe { (*pos.node_ptr()).key().clone() };

        // Defer node deletion.
        //
        self.deferred_nodes.lock().unwrap().push(pos.node_ptr());

        Some(data)
    }

    fn update(&self, new_value: T) -> bool {
        // Use the internal update method which handles forward insertion
        // Returns (old_node_ptr, new_position) - old node is orphaned and needs deferred destruction
        if let Some((old_node_ptr, _new_pos)) = self.inner.update_internal(None, new_value) {
            // Defer destruction of the old node
            self.deferred_nodes.lock().unwrap().push(old_node_ptr);
            true
        } else {
            false
        }
    }

    fn contains(&self, key: &T) -> bool {
        self.inner.contains(key)
    }

    fn find(&self, key: &T) -> Option<Self::GuardedRef<'_>> {
        let pos = self.inner.find_from_internal(None, key, true)?;
        unsafe {
            let data_ptr = self
                .inner
                .apply_on_internal(pos.node_ptr(), |x| x as *const T)?;
            let data_ref = &*data_ptr;
            Some(DeferredGuardedRef { data: data_ref })
        }
    }

    fn find_and_apply<F, R>(&self, key: &T, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        self.inner.find_and_apply(key, f)
    }

    fn is_empty(&self) -> bool {
        self.inner.first_node_internal().is_none()
    }

    fn iter(&self) -> Self::Iter<'_> {
        DeferredCollectionIter::new(self)
    }

    fn iter_from(&self, start_key: &T) -> Self::Iter<'_> {
        // First try exact match
        if let Some(pos) = self.inner.find_from_internal(None, start_key, true) {
            return DeferredCollectionIter::from_node(self, Some(pos.node_ptr()));
        }

        // Not found - find predecessor and get its successor
        let pos = self.inner.find_from_internal(None, start_key, false);

        // If we found a predecessor, get its next (which should be > start_key)
        let start_node = if let Some(pred_pos) = pos {
            self.inner.next_node_internal(pred_pos.node_ptr())
        } else {
            // No predecessor found, start from beginning and skip to first >= start_key
            let mut curr = self.inner.first_node_internal();
            while let Some(node) = curr {
                unsafe {
                    if (*node).key() >= start_key {
                        return DeferredCollectionIter::from_node(self, Some(node));
                    }
                }
                curr = self.inner.next_node_internal(node);
            }
            None
        };

        DeferredCollectionIter::from_node(self, start_node)
    }

    fn insert_batch<I>(&self, iter: I) -> usize
    where
        I: OrderedIterator<Item = T>,
    {
        self.inner.insert_batch(iter)
    }
}

impl<T, C> Default for DeferredCollection<T, C>
where
    C: SortedCollection<T> + Default,
    T: Eq + Ord,
{
    fn default() -> Self {
        Self::new(C::default())
    }
}

impl<T, C> Drop for DeferredCollection<T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
{
    fn drop(&mut self) {
        // Free all deferred nodes using proper deallocation.
        //
        // INVARIANT: Nodes in deferred_nodes must be physically unlinked from
        // the list (not just marked). This is required for epoch-based memory
        // reclamation to work correctly - a marked-but-linked node could still
        // be traversed by other threads.
        let nodes = self.deferred_nodes.get_mut().unwrap();
        for node in nodes.drain(..) {
            unsafe {
                C::Node::dealloc_ptr(node);
            }
        }
    }
}

// Thread safety
unsafe impl<T, C> Send for DeferredCollection<T, C>
where
    C: SortedCollection<T> + Send,
    T: Eq + Ord + Send,
{
}

unsafe impl<T, C> Sync for DeferredCollection<T, C>
where
    C: SortedCollection<T> + Sync,
    T: Eq + Ord + Sync,
{
}
