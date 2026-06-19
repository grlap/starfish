//! RAII owner for a freshly allocated, not-yet-published lock-free node.
//!
//! Key type: [`UnpublishedNode<N>`].

/// Owns a freshly allocated node that has **not** been CAS-published, freeing it on
/// drop via the node's own deallocator.
///
/// # Why this exists
///
/// Lock-free `insert`/`update` allocate a node and then run user-defined `Eq`/`Ord`
/// comparisons (the position search, duplicate checks, CAS-failure re-search) before
/// the publishing CAS. With the node held as a bare raw pointer, a panic in any
/// comparator unwinds past its only owner and leaks the allocation (this was
/// tracked as a leak-on-panic bug for `SortedList`/`SkipTrie`). Holding the pointer
/// in this guard instead frees it on every exit — including unwind — and relinquishes
/// ownership only at the publishing CAS via [`publish`](Self::publish).
///
/// # Immediate, never deferred
///
/// An unpublished node is **thread-private**: no other thread can ever have observed
/// it, so there is no concurrent reader to outlive. The free is therefore IMMEDIATE
/// (the node's own `dealloc`) and never routed through epoch-deferred reclamation
/// (`defer_destroy` is exclusively for nodes that *were* reachable). This guard
/// touches the epoch machinery zero times.
///
/// # Layout-agnostic
///
/// `dealloc` is the node's own `unsafe fn(*mut N)`, so the guard works for any node
/// layout — `Box`-allocated nodes (pass `CollectionNode::dealloc_ptr`) and
/// flexible-array-member nodes allocated with a custom `Layout` (pass the node's
/// `dealloc_node`) alike. `Box::from_raw` cannot free a FAM node (wrong layout), so
/// the deallocator must come from the node, not be assumed.
pub(crate) struct UnpublishedNode<N> {
    ptr: *mut N,
    dealloc: unsafe fn(*mut N),
}

impl<N> UnpublishedNode<N> {
    /// Take ownership of a freshly allocated, unpublished node.
    ///
    /// # Safety
    /// - `ptr` must be the sole owning pointer to a freshly allocated node that has
    ///   not been published (made reachable by any other thread).
    /// - `dealloc` must correctly free a node allocated that way, exactly once.
    #[inline]
    pub(crate) unsafe fn new(ptr: *mut N, dealloc: unsafe fn(*mut N)) -> Self {
        Self { ptr, dealloc }
    }

    /// The owned node pointer. Borrowing only — ownership stays with the guard.
    #[inline]
    pub(crate) fn as_ptr(&self) -> *mut N {
        self.ptr
    }

    /// Relinquish ownership: the node has been CAS-published and now belongs to the
    /// collection. Returns the raw pointer and suppresses the guard's free.
    #[inline]
    pub(crate) fn publish(self) -> *mut N {
        let ptr = self.ptr;
        core::mem::forget(self);
        ptr
    }
}

impl<N> Drop for UnpublishedNode<N> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is the sole owning pointer to an unpublished
        // (thread-private) node, and `self.dealloc` is the matching deallocator
        // supplied at construction. Freeing once here is sound; it is an immediate
        // free, not an epoch-deferred one, because no other thread can have observed
        // an unpublished node.
        unsafe { (self.dealloc)(self.ptr) }
    }
}
