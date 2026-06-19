//! Lock-free sorted linked list with Harris-style two-phase deletion.
//!
//! Implements `SortedCollection` using a singly-linked list with mark-based
//! logical deletion. Simpler than skip list but O(n) traversal.
//! Parameterized by `Guard` for memory reclamation.

use std::borrow::Borrow;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use super::load_consume;
use crate::data_structures::CollectionNode;
use crate::data_structures::MarkedPtr;
use crate::data_structures::NodePosition;
use crate::data_structures::Pair;
use crate::data_structures::hash::map_collection::{MapCollection, MapCollectionInternal, MapNode};
use crate::data_structures::internal::UnpublishedNode;
use crate::data_structures::{SortedCollection, SortedCollectionInternal};
use crate::guard::Guard;

type NodePtr<T> = *mut SortedListNode<T>;

///
/// Concurrent single list implementation based on Harris's paper 'A Pragmatic Implementation of Non-Blocking Linked-Lists'.
/// Modified to support find() and update() operations for use in SplitOrderedHashMap
///
// IMPORTANT: Memory Reclamation Issue
// ===================================
// Like the original implementation, this has a memory leak to prevent use-after-free.
// Deleted nodes are unlinked but not freed. In production, use:
// - crossbeam-epoch for epoch-based reclamation
// - hazard pointers
// - or another safe memory reclamation scheme
//
// FAILURE RECOVERY:
// =================
// When CAS operations fail during traversal (e.g., pred was marked), we restart
// from the start_node passed to node_location_from_internal. This maintains
// compatibility with split-ordered hashmap which provides bucket sentinels as
// start nodes (not HEAD).
//
// =============================================================================
// SORTED LIST INVARIANTS & REMOVE OPERATION
// =============================================================================
//
// List Structure (sorted ascending):
// ┌──────┐    ┌──────┐    ┌──────┐    ┌──────┐    ┌──────┐
// │ HEAD │───►│  10  │───►│  20  │───►│  30  │───►│ NULL │
// │(sent)│    │      │    │      │    │      │    │      │
// └──────┘    └──────┘    └──────┘    └──────┘    └──────┘
//
// Marked Pointer: The mark bit on node.next indicates the NODE is logically deleted.
//                 pred.next = (curr | MARK) means pred is marked for deletion.
//
// INVARIANTS:
// 1. List is always sorted by key (ascending)
// 2. No duplicate keys allowed
// 3. Marked nodes must be physically unlinked before Drop
// 4. HEAD sentinel is never marked or removed
//
// =============================================================================
// REMOVE OPERATION (Two-Phase Delete)
// =============================================================================
//
// Phase 1: LOGICAL DELETE (mark curr.next)
// Phase 2: PHYSICAL UNLINK (CAS pred.next from curr to curr.next)
//
// Normal Remove (no contention):
// ─────────────────────────────
// Before:  pred ──────► curr ──────► next
//
// Step 1 - Mark curr (logical delete):
//          pred ──────► curr ──╳───► next
//                              │
//                           (marked)
//
// Step 2 - Unlink (CAS pred.next from curr to next):
//          pred ─────────────────────► next
//                       curr ──╳───► next  (unlinked, will be freed)
//
// =============================================================================
// CAS FAILURE CASES IN PHYSICAL UNLINK
// =============================================================================
//
// When CAS(pred.next, curr, next) fails, we get actual = pred.next
//
// CASE 1: actual.as_ptr() == curr (but CAS failed)
// ─────────────────────────────────────────────────
// Meaning: pred is marked (actual = curr | MARK)
//
//          pred ──╳───► curr ──╳───► next
//               │            │
//            (marked)     (marked)
//
// Solution: Restart traversal from start_node to find unmarked predecessor
//
// CASE 2: actual.as_ptr() != curr AND actual.key >= curr.key
// ───────────────────────────────────────────────────────────
// Meaning: curr was already unlinked by another thread
//
// Before:  pred ──────► curr ──────► next(30)
// After:   pred ─────────────────────► next(30)
//                                      ▲
//                                   actual
//
// actual.key (30) >= curr.key (20) → curr is gone, we're done!
//
// CASE 3: actual.as_ptr() != curr AND actual.key < curr.key
// ──────────────────────────────────────────────────────────
// Meaning: A node was INSERTED between pred and curr
//
// Before:  pred ──────► curr(20) ──╳───► next
//
// After:   pred ──────► X(15) ──────► curr(20) ──╳───► next
//                       ▲
//                    actual (inserted node)
//
// actual.key (15) < curr.key (20) → insert happened!
// Solution: Advance pred to actual (the inserted node), retry CAS
//
// This case is critical: Without checking key, we might incorrectly assume
// curr was unlinked when it's actually still in the list via the inserted node.
//
// =============================================================================
// WHY INSERT CAN'T PREVENT THIS (Race Condition)
// =============================================================================
//
// Insert cannot atomically check-then-CAS:
// 1. Insert checks: curr is not marked ✓
// 2. [Window] Another thread marks curr
// 3. Insert CAS succeeds (pred.next was still curr)
// 4. Result: new_node.next → marked curr (problematic!)
//
// Therefore, REMOVE must handle this by retrying until physical unlink succeeds.
//
// =============================================================================
//
#[derive(Debug)]
pub struct SortedListNode<T> {
    data: Option<T>,
    next: AtomicPtr<SortedListNode<T>>,
}

impl<T> SortedListNode<T> {
    fn new(key: T) -> Self {
        SortedListNode {
            data: Some(key),
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn new_sentinel() -> Self {
        SortedListNode {
            data: None,
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn is_sentinel(&self) -> bool {
        self.data.is_none()
    }

    // =========================================================================
    // Next pointer accessors
    // =========================================================================

    /// Load next pointer (consume ordering).
    ///
    /// Uses load_consume: Relaxed + compiler_fence on ARM (plain `ldr`),
    /// Acquire on x86 (plain `mov` under TSO).
    #[inline]
    pub(super) fn get_next(&self) -> NodePtr<T> {
        load_consume(&self.next)
    }

    /// Store next pointer (Relaxed ordering)
    /// Used for initial setup of unpublished nodes before CAS-linking.
    /// Safe because the CAS that publishes the node provides Release semantics.
    #[inline]
    fn set_next_relaxed(&self, ptr: NodePtr<T>) {
        self.next.store(ptr, Ordering::Relaxed)
    }

    /// CAS next pointer (Release/Acquire ordering)
    #[inline]
    fn cas_next(&self, expected: NodePtr<T>, new: NodePtr<T>) -> Result<NodePtr<T>, NodePtr<T>> {
        // Acquire on failure is required because `unlink_marked_node` may
        // dereference the returned pointer immediately after a failed CAS.
        // (unsound on weakly-ordered architectures without it).
        self.next
            .compare_exchange(expected, new, Ordering::Release, Ordering::Acquire)
    }

    /// Weak CAS next pointer (Release/Acquire ordering)
    #[inline]
    fn cas_next_weak(
        &self,
        expected: NodePtr<T>,
        new: NodePtr<T>,
    ) -> Result<NodePtr<T>, NodePtr<T>> {
        // Same rationale as `cas_next`: callers may inspect the failure value
        // as a published node pointer before retrying.
        self.next
            .compare_exchange_weak(expected, new, Ordering::Release, Ordering::Acquire)
    }
}

impl<T> CollectionNode<T> for SortedListNode<T> {
    fn key(&self) -> &T {
        self.data
            .as_ref()
            .expect("Cannot get key from sentinel node")
    }
}

// Internal traversal helper — Copy is intentional (short-lived, single-call scope).
#[derive(Debug, Copy, Clone)]
struct NodeLocation<T> {
    pub pred: NodePtr<T>,
    pub curr: NodePtr<T>,
}

/// Position in a SortedList containing predecessor and current node.
///
/// For a single linked list, we only need one predecessor pointer.
/// This enables O(1) amortized batch inserts when iterating through sorted data.
///
pub struct ListNodePosition<T> {
    /// Predecessor node (for batch insert optimization)
    pred: NodePtr<T>,
    /// Current node at this position
    node: NodePtr<T>,
}

// No Copy/Clone: positions hold raw pointers and must not outlive
// the guard epoch in which they were created (move-only).

impl<T> NodePosition<T> for ListNodePosition<T> {
    type Node = SortedListNode<T>;

    fn node(&self) -> Option<*mut Self::Node> {
        if self.node.is_null() {
            None
        } else {
            Some(self.node)
        }
    }

    fn node_ptr(&self) -> *mut Self::Node {
        self.node
    }

    fn empty() -> Self {
        ListNodePosition {
            pred: ptr::null_mut(),
            node: ptr::null_mut(),
        }
    }

    fn from_node(node: *mut Self::Node) -> Self {
        ListNodePosition {
            pred: ptr::null_mut(),
            node,
        }
    }

    fn is_valid(&self) -> bool {
        !self.node.is_null()
    }
}

impl<T> ListNodePosition<T> {
    /// Create a new position with both predecessor and node
    pub fn new(pred: NodePtr<T>, node: NodePtr<T>) -> Self {
        ListNodePosition { pred, node }
    }

    /// Get the predecessor pointer (for batch operations)
    pub fn pred(&self) -> NodePtr<T> {
        self.pred
    }

    /// Get the best starting node for a traversal: predecessor if available, otherwise the node.
    pub fn start_node(&self) -> Option<NodePtr<T>> {
        if !self.pred.is_null() {
            Some(self.pred)
        } else {
            self.node()
        }
    }
}

pub struct SortedList<T, G: Guard> {
    pub(crate) head: AtomicPtr<SortedListNode<T>>,
    /// Shared guard instance for deferred destruction.
    /// All deleted nodes are deferred to this guard and freed when it drops.
    guard: G,
}

impl<T, G> SortedList<T, G>
where
    T: Eq + Ord,
    G: Guard,
{
    pub fn new() -> Self {
        // Create sentinel head node without a value.
        //
        let head_node = Box::into_raw(Box::new(SortedListNode::new_sentinel()));
        SortedList {
            head: AtomicPtr::new(head_node),
            guard: G::default(),
        }
    }

    /// Get the shared guard instance for this collection.
    /// Used by SortedCollection trait methods for deferred destruction.
    pub fn guard(&self) -> &G {
        &self.guard
    }

    /// Unlinks a marked node from the list, guaranteeing completion before return.
    ///
    /// After marking a node (DELETE or UPDATE), this function snips it out
    /// by CASing pred.next from `marked_node` to `replacement`.
    ///
    /// For DELETE: replacement = marked_node.next.as_ptr() (the successor)
    /// For UPDATE: replacement = new_node (the replacement node)
    ///
    /// Unlike try_unlink, this function loops until the node is confirmed unlinked.
    /// This is REQUIRED for safe epoch-based reclamation - nodes must be fully
    /// unlinked before being deferred for deallocation.
    ///
    /// Returns the final predecessor node (may differ from initial pred if retries occurred).
    ///
    /// # Safety
    /// - `marked_node` and `replacement` must be valid pointers
    /// - `marked_node` must already be marked (DELETE or UPDATE)
    /// - `start_node` must be a valid starting point for traversal (or None for HEAD)
    /// - The caller must hold a pinned epoch guard; this function traverses and
    ///   CAS-retries against nodes that a concurrent thread may be reclaiming.
    unsafe fn unlink_marked_node(
        &self,
        mut pred: NodePtr<T>,
        marked_node: NodePtr<T>,
        replacement: NodePtr<T>,
        start_node: Option<NodePtr<T>>,
    ) -> NodePtr<T>
    where
        T: Ord,
    {
        // SAFETY: `marked_node` is a valid, allocated node per the function's safety contract.
        // The caller guarantees it is pinned by a memory reclamation guard, preventing reclamation.
        let key = unsafe { (*marked_node).key() };
        // Shadow parameter with mutable local - once invalid, stays None
        let mut start_node = start_node;

        loop {
            // Try to unlink: CAS pred.next from marked_node to replacement
            // SAFETY: `pred` is valid — it was either passed in by the caller (safety contract)
            // or found via traversal from HEAD under the guard, preventing reclamation.
            let cas_result = unsafe { (*pred).cas_next(marked_node, replacement) };

            if cas_result.is_ok() {
                // Successfully unlinked
                return pred;
            }

            // CAS failed - need to find a new valid predecessor
            // This can happen if:
            // 1. pred was marked (deleted by another thread)
            // 2. Something was inserted between pred and marked_node
            // 3. Another thread already unlinked marked_node (we're done!)

            // Check if marked_node was already unlinked by examining the actual value
            let actual = cas_result.unwrap_err();
            let actual_ptr = MarkedPtr::unmask(actual);

            // If pred.next no longer points to marked_node and points to something
            // with key >= marked_node's key, the node was already unlinked.
            //
            // SOUNDNESS (vs the multi-level skip-list bug this resembles): drawing
            // "already unlinked" from a key comparison is safe HERE even if `actual`
            // came from a marked (frozen) pred slot, because a single-level list has
            // in-edge continuity — a node observed linked stays linked until the one
            // edge pointing at it is CASed away, so a frozen snapshot that sorts past
            // `marked_node` can only exist if `marked_node` was already unlinked
            // through that very edge. Multi-level towers have NO such continuity (a
            // node can link into level L after a neighbor's level-L slot froze),
            // which is why `SkipList`/`SkipTrie::unlink_at_level` must recover off
            // marked slots before concluding anything.
            if actual_ptr != marked_node {
                if actual_ptr.is_null() {
                    // pred.next is null - marked_node was definitely unlinked
                    return pred;
                }
                // SAFETY: `actual_ptr` is non-null (checked above), unmasked, and was loaded from
                // pred.next via CAS failure — it is a live node protected by the guard.
                let actual_key = unsafe { (*actual_ptr).key() };
                if actual_key > key {
                    // pred.next points past marked_node - already unlinked
                    return pred;
                }
                // actual_key == key but different node: might be concurrent INSERT,
                // need to traverse to check if our marked_node is still in the list
                // actual_key < key: something was inserted between pred and marked_node
                // Need to find the new predecessor
            }

            // Traverse from start to find the current predecessor of marked_node
            // IMPORTANT: We can't use start_node if it:
            // 1. IS the marked_node itself (we'd start after the node we're looking for)
            // 2. Is marked for deletion (invalid starting point)
            // In these cases, fall back to HEAD.
            let mut start = match start_node {
                Some(s) => {
                    let s_clean = MarkedPtr::unmask(s);
                    if s_clean == marked_node {
                        // Can't start from the node we're trying to unlink!
                        // Invalidate for future iterations
                        start_node = None;
                        self.head.load(Ordering::Acquire)
                    } else {
                        // Check if start_node is marked (being deleted)
                        // SAFETY: `s_clean` is unmasked and non-null (not equal to marked_node).
                        // It was provided by the caller as a valid starting point under the guard.
                        let s_next = unsafe { (*s_clean).get_next() };
                        if MarkedPtr::new(s_next).is_any_marked() {
                            // Start node is being deleted, use HEAD
                            // Invalidate for future iterations
                            start_node = None;
                            self.head.load(Ordering::Acquire)
                        } else {
                            s_clean
                        }
                    }
                }
                None => self.head.load(Ordering::Acquire),
            };

            pred = start;
            // SAFETY: `pred` is either the HEAD sentinel (always valid) or a validated
            // start_node. Both are valid, allocated nodes protected by the guard.
            let mut curr = unsafe { (*pred).get_next() };

            loop {
                curr = MarkedPtr::unmask(curr);

                if curr.is_null() {
                    // Reached end without finding marked_node - it's already unlinked
                    return pred;
                }

                if curr == marked_node {
                    // Found it - pred is the current predecessor, retry CAS
                    break;
                }

                // SAFETY: `curr` is non-null (checked above), unmasked, and was reached by
                // traversal from HEAD/start_node. The guard prevents reclamation.
                let next = unsafe { (*curr).get_next() };
                let next_marked = MarkedPtr::new(next);

                // Skip over any marked nodes during traversal
                if next_marked.is_any_marked() {
                    // Try to help snip this marked node
                    // SAFETY: `pred` is a valid node — it was set from `start` (HEAD or validated
                    // start_node) and advanced only through live, unmasked nodes during traversal.
                    let snip_result = unsafe { (*pred).cas_next(curr, next_marked.as_ptr()) };

                    if snip_result.is_err() {
                        // Snip failed - pred might be marked (being deleted by another thread)
                        // Check if pred.next is marked, indicating pred is being removed
                        // SAFETY: `pred` is valid — same reasoning as the CAS above.
                        let pred_next_raw = unsafe { (*pred).get_next() };
                        if MarkedPtr::new(pred_next_raw).is_any_marked() {
                            // pred is being deleted, need to restart from a valid node
                            // Try start first (more efficient), fall back to HEAD if start is also marked
                            // SAFETY: `start` is either HEAD (always valid) or a validated start_node
                            // from the outer match. Protected by the guard.
                            let start_next = unsafe { (*start).get_next() };
                            if MarkedPtr::new(start_next).is_any_marked() {
                                // start is also invalid, update it to HEAD for all future restarts
                                // Also invalidate start_node for future outer loop iterations
                                start_node = None;
                                start = self.head.load(Ordering::Acquire);
                            }
                            pred = start;
                            // SAFETY: `pred` is HEAD (just assigned above) or a validated
                            // start_node — always a valid, allocated sentinel or list node.
                            curr = unsafe { (*pred).get_next() };
                            continue;
                        }
                    }

                    // SAFETY: `pred` is valid — it is either HEAD, a validated start_node,
                    // or a node reached via traversal. Protected by the guard.
                    curr = unsafe { (*pred).get_next() };
                    continue;
                }

                // Check if we've passed marked_node's position
                // SAFETY: `curr` is non-null (checked at loop top), unmasked, and reached via
                // traversal from HEAD/start. The guard prevents reclamation.
                let curr_key = unsafe { (*curr).key() };
                if curr_key > key {
                    // We're past where marked_node could be - it's already unlinked
                    return pred;
                }
                // curr_key == key but curr != marked_node: concurrent INSERT created
                // a node with the same key, our marked_node might still be after it

                pred = curr;
                curr = next;
            }
            // Loop back to retry CAS with new pred
        }
    }

    // Core operation: Find with cleanup
    // Returns NodeLocation (pred_node, curr_node, next_node)
    //
    // When CAS fails during snipping, we restart from start_node (not HEAD).
    // This maintains compatibility with split-ordered hashmap which provides
    // bucket sentinels as start nodes.
    //
    // INVARIANT: All returned pointers (pred, curr) are clean (unmasked).
    // Callers may rely on this without additional unmasking.
    //
    // Precondition: the caller must hold a pinned epoch guard for the duration
    // of this call. Without it, concurrent reclamation may free nodes that
    // this traversal is still reading.
    //
    fn node_location_from_internal<Q>(
        &self,
        key: &Q,
        head_node: Option<NodePtr<T>>,
    ) -> NodeLocation<T>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        'retry: loop {
            let mut pred_node = match head_node {
                Some(start_node) => MarkedPtr::unmask(start_node),
                None => self.head.load(Ordering::Acquire),
            };

            // SAFETY: `pred_node` is either HEAD (loaded with Acquire) or an unmasked
            // caller-provided start_node. Both are valid, allocated nodes. The caller
            // holds a pinned guard, preventing reclamation.
            let mut curr_node = unsafe { (*pred_node).get_next() };

            loop {
                curr_node = MarkedPtr::unmask(curr_node);

                if curr_node.is_null() {
                    debug_assert!(!MarkedPtr::new(pred_node).is_any_marked());
                    return NodeLocation {
                        pred: pred_node,
                        curr: curr_node,
                    };
                }

                // SAFETY: `curr_node` is non-null (checked above) and unmasked.
                // It was reached by following next pointers from a valid node under the guard.
                let next_node = unsafe { (*curr_node).get_next() };
                let next_marked = MarkedPtr::new(next_node);

                if next_marked.is_any_marked() {
                    // Try to physically remove the marked node (DELETE or UPDATE).
                    // For both: snip curr out by pointing pred to curr's successor.
                    // For UPDATE-marked: next_marked.as_ptr() is the replacement node.
                    //
                    // SAFETY: `pred_node` is valid — it started as HEAD or start_node and was
                    // only advanced to unmasked, unmarked nodes during traversal. Guard-protected.
                    let snip = unsafe { (*pred_node).cas_next(curr_node, next_marked.as_ptr()) };

                    if snip.is_err() {
                        // CAS failed - pred_node.next != curr_node
                        // Either someone else snipped, or pred was marked.
                        // Restart from start_node to find a valid path.
                        //
                        continue 'retry;
                    }

                    // Successfully snipped, continue with next node
                    curr_node = next_marked.as_ptr();
                } else {
                    // Node is not marked, check if we found position
                    // SAFETY: `curr_node` is non-null, unmasked, and its next pointer was just
                    // verified unmarked. It is a live node protected by the guard.
                    unsafe {
                        if !(*curr_node).is_sentinel() && (*curr_node).key().borrow() >= key {
                            // Before returning, double-check the node isn't marked.
                            //
                            let recheck = (*curr_node).get_next();
                            if MarkedPtr::new(recheck).is_any_marked() {
                                // Node got marked while we were checking - restart
                                continue 'retry;
                            }
                            debug_assert!(!MarkedPtr::new(pred_node).is_any_marked());
                            debug_assert!(!MarkedPtr::new(curr_node).is_any_marked());
                            return NodeLocation {
                                pred: pred_node,
                                curr: curr_node,
                            };
                        }
                    }

                    pred_node = curr_node;
                    curr_node = next_marked.as_ptr();
                }
            }
        }
    }
}

/// Generic key-lookup helpers using Borrow-based lookups.
///
impl<T: Ord, G: Guard> SortedList<T, G> {
    /// Finds the node whose borrow equals `key` and returns its pointer.
    ///
    /// # Precondition
    ///
    /// The caller **must** hold a pinned epoch guard (`G::pin()`) for the
    /// duration of this call.
    pub(crate) fn find_by_internal<Q>(&self, key: &Q) -> Option<NodePtr<T>>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let loc = self.node_location_from_internal(key, None);
        if loc.curr.is_null() {
            return None;
        }
        // SAFETY: `loc.curr` is non-null (checked above) and was returned by
        // `node_location_from_internal`, which guarantees it is a valid, unmasked, live node.
        if unsafe { (*loc.curr).key().borrow() == key } {
            Some(loc.curr)
        } else {
            None
        }
    }

    /// Removes the element whose borrow equals `key`.
    /// Returns the removed node pointer if found, None otherwise.
    ///
    /// Core CAS-mark-snip loop for node removal. Returns `(final_pred, curr)`
    /// on success.
    ///
    /// The caller **must** hold a pinned epoch guard (`G::pin()`).
    fn remove_internal<Q>(
        &self,
        key: &Q,
        start_node: Option<NodePtr<T>>,
    ) -> Option<(NodePtr<T>, NodePtr<T>)>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        loop {
            let location = self.node_location_from_internal(key, start_node);
            let (pred, curr) = (location.pred, location.curr);
            debug_assert!(!MarkedPtr::new(pred).is_any_marked());

            if curr.is_null() {
                return None;
            }

            // SAFETY: `curr` is non-null (checked above), returned by `node_location_from_internal`
            // which guarantees it is valid, unmasked, and live. The caller holds a guard.
            unsafe {
                if (*curr).key().borrow() != key {
                    return None;
                }

                let curr_next = (*curr).get_next();
                let curr_next_marked = MarkedPtr::new(curr_next);

                // If DELETE-marked, already deleted. If UPDATE-marked, retry
                // to find the replacement node.
                if curr_next_marked.is_any_marked() {
                    if curr_next_marked.is_delete_marked() {
                        return None;
                    }
                    continue;
                }

                // Try to mark for deletion.
                let marked = curr_next_marked.with_mark(true);
                if (*curr).cas_next_weak(curr_next, marked.as_raw()).is_err() {
                    continue;
                }

                // Successfully marked — unlink before return (required for epoch reclamation).
                let successor = curr_next_marked.as_ptr();
                let final_pred = self.unlink_marked_node(pred, curr, successor, start_node);
                return Some((final_pred, curr));
            }
        }
    }
}

/// Wrap a freshly allocated, unpublished `SortedListNode` in the shared
/// [`UnpublishedNode`] guard (frees on unwind; relinquish at the publishing CAS). The
/// node is `Box`-allocated, so its deallocator is `CollectionNode::dealloc_ptr`.
#[inline]
fn unpublished_node<T>(node: SortedListNode<T>) -> UnpublishedNode<SortedListNode<T>> {
    // SAFETY: `Box::into_raw` yields the sole owning pointer to a fresh, unpublished
    // node, and `dealloc_ptr` (a `Box::from_raw` drop) is its matching deallocator.
    unsafe {
        UnpublishedNode::new(
            Box::into_raw(Box::new(node)),
            <SortedListNode<T> as CollectionNode<T>>::dealloc_ptr,
        )
    }
}

/// Implement SortedCollectionInternal for SortedList.
///
impl<T, G> SortedCollectionInternal<T> for SortedList<T, G>
where
    T: Eq + Ord,
    G: Guard,
{
    type Guard = G;
    type Node = SortedListNode<T>;
    type NodePosition = ListNodePosition<T>;

    fn guard(&self) -> &G {
        &self.guard
    }

    /// Internal insert that optionally starts from a position.
    ///
    unsafe fn insert_from_internal(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition> {
        // RAII-owned until published: a panic in a user `Eq`/`Ord` comparison below
        // (position search / duplicate checks) frees the node instead of leaking it.
        // Each early `return None` drops the guard too.
        let new = unpublished_node(SortedListNode::new(key));

        loop {
            // SAFETY: `new` owns a live, exclusively-owned (unpublished) node.
            let key = unsafe { (*new.as_ptr()).key() };

            let start_node = position.and_then(|pos| pos.start_node());

            // Check if start_node itself has the same key (duplicate check for hint-based insert).
            // This prevents inserting duplicates when the hint node IS the duplicate.
            //
            if let Some(hint) = start_node {
                let hint = MarkedPtr::unmask(hint);
                // SAFETY: `hint` is an unmasked, non-null pointer from the caller's position.
                // The caller holds a guard, ensuring the node remains valid.
                unsafe {
                    if !(*hint).is_sentinel() && (*hint).key() == key {
                        return None; // Duplicate — `new` drops, freeing the unused node.
                    }
                }
            }

            let loc = self.node_location_from_internal(key, start_node);
            let (pred, curr) = (loc.pred, loc.curr);
            debug_assert!(!MarkedPtr::new(pred).is_any_marked());

            // Check for duplicate.
            //
            if !curr.is_null() {
                // SAFETY: `curr` is non-null (checked above) and was returned by
                // `node_location_from_internal` — a valid, unmasked, live node under the guard.
                unsafe {
                    if (*curr).key() == key {
                        return None; // Duplicate — `new` drops, freeing the unused node.
                    }
                }
            }

            // Relaxed: node is unpublished; the CAS below provides Release.
            // SAFETY: `new` owns the node exclusively (not yet published). Writing to
            // it is safe because no other thread can observe it.
            unsafe {
                (*new.as_ptr()).set_next_relaxed(curr);
            }

            // Try to link new node
            // SAFETY: `pred` is valid — returned by `node_location_from_internal` as an unmasked,
            // live node. The guard prevents reclamation during the CAS.
            let result = unsafe { (*pred).cas_next_weak(curr, new.as_ptr()) };

            if result.is_ok() {
                // Published: relinquish ownership so the guard does not free the now-live node.
                let new_node = new.publish();
                // Return position with predecessor for O(1) batch operations
                return Some(ListNodePosition::new(pred, new_node));
            }
            // CAS failed, retry (guard still owns the unpublished node)
        }
    }

    /// Removes a node starting from a position and return position if it existed.
    ///
    unsafe fn remove_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
    ) -> Option<Self::NodePosition> {
        let start_node = position.and_then(|pos| pos.start_node());
        self.remove_internal(key, start_node)
            .map(|(pred, curr)| ListNodePosition::new(pred, curr))
    }

    unsafe fn find_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
        is_match: bool,
    ) -> Option<Self::NodePosition> {
        let start_node = position.and_then(|pos| pos.start_node());

        let location = self.node_location_from_internal(key, start_node);
        debug_assert!(!MarkedPtr::new(location.pred).is_any_marked());

        if location.curr.is_null() {
            return None;
        }

        let node = location.curr;

        // Check the key (no sentinel check needed).
        //
        if is_match {
            // SAFETY: `node` is non-null (checked above) and was returned by
            // `node_location_from_internal` — a valid, unmasked, live node under the guard.
            if unsafe { (*node).key().borrow() == key } {
                return Some(ListNodePosition::new(location.pred, node));
            } else {
                return None;
            }
        }

        Some(ListNodePosition::new(location.pred, location.pred))
    }

    /// Apply a function to the given node's value.
    ///
    /// If we have a pointer to a node, use it directly - no need to follow UPDATE marks.
    /// UPDATE-mark following is only needed during traversal to find a node by key.
    ///
    /// # Precondition
    ///
    /// The caller must hold a pinned epoch guard. `node` is a raw pointer obtained
    /// from a prior traversal; without a guard it may be freed by the time this
    /// function dereferences it.
    unsafe fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        let curr = MarkedPtr::unmask(node);

        if curr.is_null() {
            return None;
        }

        // SAFETY: `curr` is non-null (checked above) and unmasked. The caller holds a
        // pinned guard (per precondition), so the node cannot be reclaimed. The
        // shared reference is valid because we only read and no mutable alias exists.
        unsafe {
            let node_ref = &*curr;

            if node_ref.is_sentinel() {
                return None;
            }

            Some(f(node_ref.key()))
        }
    }

    /// # Precondition
    ///
    /// The caller must hold a pinned epoch guard for the duration of this call.
    unsafe fn first_node_internal(&self) -> Option<*mut Self::Node> {
        let head = self.head.load(Ordering::Acquire);
        // SAFETY: `head` was loaded from `self.head` with Acquire ordering. The HEAD sentinel
        // is allocated in `new()` and never deallocated until `Drop`. Always valid.
        let mut curr = unsafe { (*head).get_next() };

        while !curr.is_null() {
            // SAFETY: `curr` is non-null (loop condition) and was reached by following next
            // pointers from the valid HEAD node. The guard prevents reclamation.
            let marked = MarkedPtr::new(unsafe { (*curr).get_next() });

            // Unmarked node — this is a live node, return it.
            // Marked nodes (DELETE or UPDATE) are followed via as_ptr():
            // DELETE-marked nodes are skipped, UPDATE-marked nodes are followed
            // to their replacement (forward insertion places it at curr.next).
            if !marked.is_any_marked() {
                return Some(curr);
            }

            curr = marked.as_ptr();
        }

        None
    }

    /// # Precondition
    ///
    /// The caller must hold a pinned epoch guard for the duration of this call.
    unsafe fn next_node_internal(&self, node: *mut Self::Node) -> Option<*mut Self::Node> {
        if node.is_null() {
            return None;
        }

        let node = MarkedPtr::unmask(node);

        // SAFETY: `node` is non-null (checked above) and unmasked. The caller holds a pinned
        // guard (per precondition), and all nodes reached via next pointers are protected
        // from reclamation. No mutable aliases exist — we only read through shared references.
        unsafe {
            // Key just yielded for `node`. A forward-insertion UPDATE links `node`'s NEW
            // same-key replacement immediately after it and UPDATE-marks `node`; returning
            // that replacement would yield the key TWICE. Keys are unique, so any
            // node reached here whose key equals `node`'s is a transient UPDATE replacement —
            // skip it (this also covers update-during-update chains). The genuine successor has
            // a strictly greater key. (Point reads are unaffected; only forward iteration must
            // distinguish "my replacement" from "my successor".)
            // The HEAD sentinel carries NO key — reading it panics. It is a
            // legitimate input: `find_from_internal(.., is_match = false)` returns the
            // PREDECESSOR position, which is HEAD whenever the search key precedes every
            // element (the `range(start..)` below-minimum shape). Nothing was yielded
            // for it, so there is no same-key replacement to skip.
            let node_key = if (*node).is_sentinel() {
                None
            } else {
                Some((*node).key())
            };
            let mut curr = (*node).get_next();

            while !curr.is_null() {
                curr = MarkedPtr::unmask(curr);

                if curr.is_null() {
                    return None;
                }

                // Return `curr` only if it is live (its own `next` unmarked) AND its key
                // differs from the one just yielded. Otherwise skip: DELETE/UPDATE-marked
                // nodes, and same-key replacements of `node`.
                let next_marked = MarkedPtr::new((*curr).get_next());
                if !next_marked.is_any_marked() && node_key.is_none_or(|k| (*curr).key() != k) {
                    return Some(curr);
                }

                curr = next_marked.as_ptr();
            }
        }

        None
    }

    /// Update a value by replacing the node atomically using UPDATE_MARK.
    ///
    /// This approach keeps the key always available during updates:
    /// - Readers encountering an UPDATE-marked node follow curr.next to the new node
    /// - The key is never "unavailable" - readers always find either old or new value
    ///
    /// Algorithm (Forward Insertion - no backlinks needed):
    /// 1. Find pred and curr where curr.key == key
    /// 2. Create new_node with new_value
    /// 3. Set new_node.next = curr.next (the successor)
    /// 4. CAS curr.next: succ -> (new_node | UPDATE_MARK) [LINEARIZATION POINT]
    ///    - This atomically inserts new_node AND marks curr as updated
    /// 5. CAS pred.next: curr -> new_node (snip curr out)
    /// 6. Return position of new_node
    ///
    /// Key insight: Forward insertion - new_node is inserted right after curr.
    /// The UPDATE-marked curr.next points directly to new_node. No backlinks needed.
    /// Readers seeing UPDATE-marked curr just follow curr.next to find new_node.
    ///
    unsafe fn update_internal(
        &self,
        position: Option<&Self::NodePosition>,
        new_value: T,
    ) -> Option<Self::NodePosition> {
        // RAII-owned until published: a panic in a user `Eq`/`Ord` comparison (the
        // position search / key match below) frees the node instead of leaking it.
        // The not-found / key-mismatch `return None` paths drop it too.
        let new = unpublished_node(SortedListNode::new(new_value));

        // Use the new node's key for lookups
        // SAFETY: `new` owns a live, exclusively-owned (unpublished) node.
        let key = unsafe { (*new.as_ptr()).key() };

        loop {
            let start_node = position.and_then(|pos| pos.start_node());

            let location = self.node_location_from_internal(key, start_node);
            let (pred, curr) = (location.pred, location.curr);
            debug_assert!(!MarkedPtr::new(pred).is_any_marked());

            // Key not found
            if curr.is_null() {
                return None; // `new` drops, freeing the unpublished node.
            }

            // SAFETY: `curr` is non-null (checked above), returned by `node_location_from_internal`
            // as a valid, unmasked, live node. `new` owns its node exclusively (unpublished).
            // The caller holds a guard, preventing reclamation of traversed nodes.
            unsafe {
                // Verify key matches
                if (*curr).key() != key {
                    return None; // `new` drops, freeing the unpublished node.
                }

                // Get curr's next pointer (the successor)
                let curr_next = (*curr).get_next();
                let curr_next_marked = MarkedPtr::new(curr_next);

                // If curr is already DELETE-marked, someone else is deleting it
                if curr_next_marked.is_delete_marked() {
                    continue; // Retry - will find a different node or none
                }

                // If curr is already UPDATE-marked, another update is in progress
                // In forward insertion approach, curr.next already points to new value
                if curr_next_marked.is_update_marked() {
                    continue; // Retry - the key might be gone or we'll find the new node
                }

                // Relaxed: node is unpublished; the CAS below provides Release.
                let succ = curr_next_marked.as_ptr();
                (*new.as_ptr()).set_next_relaxed(succ);

                // LINEARIZATION POINT: CAS curr.next from succ to (new_node | UPDATE_MARK)
                // This atomically:
                // 1. Inserts new_node right after curr (curr -> new_node -> succ)
                // 2. Marks curr as UPDATE-marked
                // Readers finding UPDATE-marked curr will follow curr.next to find new_node
                let update_marked_new_node =
                    MarkedPtr::new(new.as_ptr()).with_update_mark(true).as_raw();
                let mark_result = (*curr).cas_next_weak(curr_next, update_marked_new_node);

                if mark_result.is_err() {
                    // Someone modified curr.next, retry
                    continue;
                }

                // Successfully UPDATE-marked with new_node inserted! Published — so
                // relinquish ownership; the node is now the list's.
                // The UPDATE is logically complete (linearization point was the CAS above).
                let new_node = new.publish();
                // Now unlink curr - this MUST complete before return.
                // Required for safe epoch-based reclamation.
                let final_pred = self.unlink_marked_node(pred, curr, new_node, start_node);
                // RETIREMENT (internal, per the trait contract): `unlink_marked_node`
                // loops until `curr` is confirmed unreachable from HEAD, so deferring
                // its destruction here is epoch-safe. Callers never retire.
                self.guard
                    .defer_destroy(curr, <Self::Node as CollectionNode<T>>::dealloc_ptr);
                return Some(ListNodePosition::new(final_pred, new_node));
            }
        }
    }
}

impl<T, G> SortedCollection<T> for SortedList<T, G>
where
    T: Eq + Ord,
    G: Guard,
{
}

// ============================================================================
// MapNode<K, V> impl
// ============================================================================

impl<K: Eq, V> MapNode<K, V> for SortedListNode<Pair<K, V>> {
    #[inline]
    fn key(&self) -> &K {
        &CollectionNode::key(self).key
    }

    #[inline]
    fn value(&self) -> Option<&V> {
        Some(&CollectionNode::key(self).value)
    }

    unsafe fn dealloc_ptr(ptr: *mut Self) {
        // SortedListNode uses Box allocation — delegate to CollectionNode's default.
        // SAFETY: The caller guarantees `ptr` was allocated via `Box::into_raw` and that
        // ownership has been transferred (no other references or aliases exist).
        unsafe { CollectionNode::dealloc_ptr(ptr) }
    }
}

// ============================================================================
// MapCollection<K, V> impl
// ============================================================================

impl<K, V, G: Guard> MapCollectionInternal<K, V> for SortedList<Pair<K, V>, G>
where
    K: Eq + Ord,
{
    type Guard = G;
    type Node = SortedListNode<Pair<K, V>>;

    fn guard(&self) -> &Self::Guard {
        &self.guard
    }

    fn insert_internal(&self, key: K, value: V) -> Option<*mut Self::Node> {
        // SAFETY: guard pinned by caller (MapCollection::insert).
        let pos = unsafe {
            SortedCollectionInternal::insert_from_internal(self, Pair::new(key, value), None)
        };
        pos.map(|p| p.node_ptr())
    }

    fn remove_internal(&self, key: &K) -> Option<*mut Self::Node> {
        SortedList::remove_internal(self, key, None).map(|(_, curr)| curr)
    }

    fn find_internal(&self, key: &K) -> Option<*mut Self::Node> {
        self.find_by_internal(key)
    }

    fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
        let curr = MarkedPtr::unmask(node);
        if curr.is_null() {
            return None;
        }
        // SAFETY: `curr` is non-null (checked above) and unmasked. The caller holds a pinned
        // guard, so the node cannot be reclaimed. We only take a shared reference.
        unsafe {
            let node_ref = &*curr;
            if node_ref.is_sentinel() {
                return None;
            }
            let pair = CollectionNode::key(node_ref);
            Some(f(&pair.key, &pair.value))
        }
    }

    fn len_internal(&self) -> usize {
        // Walk the list counting live nodes. A node is live if its own next
        // pointer is unmarked (not DELETE-marked or UPDATE-marked).
        let mut count = 0;
        let head = self.head.load(Ordering::Acquire);
        // SAFETY: `head` is the HEAD sentinel, allocated in `new()` and valid until `Drop`.
        let mut curr = unsafe { MarkedPtr::new((*head).get_next()).as_ptr() };
        while !curr.is_null() {
            // SAFETY: `curr` is non-null (loop condition) and was reached by following next
            // pointers from the valid HEAD sentinel. The guard prevents reclamation.
            unsafe {
                let curr_next = MarkedPtr::new((*curr).get_next());
                if !curr_next.is_any_marked() {
                    count += 1;
                }
                curr = curr_next.as_ptr();
            }
        }
        count
    }

    fn update_internal(&self, key: K, value: V) -> bool {
        // SAFETY: guard pinned by caller (MapCollection::update). The replaced node is
        // unlinked AND retired internally by the node-replacement update.
        unsafe {
            SortedCollectionInternal::update_internal(self, None, Pair::new(key, value)).is_some()
        }
    }
}

impl<K, V, G: Guard> MapCollection<K, V> for SortedList<Pair<K, V>, G>
where
    K: Eq + Ord,
{
    fn is_empty(&self) -> bool {
        let _guard = G::pin();
        // SAFETY: guard pinned above for the duration of this call.
        unsafe { self.first_node_internal() }.is_none()
    }
}

impl<T, G> Default for SortedList<T, G>
where
    T: Eq + Ord,
    G: Guard,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T, G: Guard> Drop for SortedList<T, G> {
    fn drop(&mut self) {
        // Clean up all nodes including sentinel.
        //
        let mut curr = self.head.load(Ordering::Acquire);

        while !curr.is_null() {
            // SAFETY: `curr` is non-null (loop condition). During `drop(&mut self)` we have
            // exclusive access (&mut self), so no concurrent readers exist. Each node was
            // allocated via `Box::into_raw` and we have ownership to deallocate it.
            unsafe {
                let next_raw = (*curr).get_next();
                let next_marked = MarkedPtr::new(next_raw);

                // Check invariant: no DELETE-marked nodes should exist at drop time.
                // DELETE-marked nodes indicate incomplete physical unlinking.
                // UPDATE-marked nodes are allowed as they'll be cleaned up normally.
                if next_marked.is_delete_marked() && !(*curr).is_sentinel() {
                    panic!(
                        "INVARIANT VIOLATION: Found DELETE-marked node at drop time!\n\
                         DELETE-marked nodes should have been physically unlinked before drop."
                    );
                }

                let next = next_marked.as_ptr();
                CollectionNode::dealloc_ptr(curr);

                curr = next;
            }
        }
    }
}

// ============================================================================
// Tests - Unique to SortedList
// ============================================================================
// Note: Common tests are in tests/deferred_collection_tests.rs

#[cfg(test)]
mod tests {
    use crate::guard::DeferredGuard;

    use super::*;
    use std::sync::Arc;
    use std::thread;

    // Positions must be move-only to prevent stale pointer reuse across guard epochs.
    static_assertions::assert_not_impl_any!(ListNodePosition<i32>: Copy, Clone);

    /// Key type that counts live instances and panics in its comparators on demand.
    /// Lets a leak test prove that a freshly allocated, unpublished node is freed
    /// (its payload dropped, count balanced) when a user `Eq`/`Ord` unwinds.
    struct PanickingKey(i32);
    impl PanickingKey {
        fn new(v: i32) -> Self {
            LIVE_KEYS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            PanickingKey(v)
        }
    }
    impl Drop for PanickingKey {
        fn drop(&mut self) {
            LIVE_KEYS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    impl PartialEq for PanickingKey {
        fn eq(&self, o: &Self) -> bool {
            assert!(
                !PANIC_ARMED.load(std::sync::atomic::Ordering::SeqCst),
                "boom (Eq)"
            );
            self.0 == o.0
        }
    }
    impl Eq for PanickingKey {}
    impl PartialOrd for PanickingKey {
        fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(o))
        }
    }
    impl Ord for PanickingKey {
        fn cmp(&self, o: &Self) -> std::cmp::Ordering {
            assert!(
                !PANIC_ARMED.load(std::sync::atomic::Ordering::SeqCst),
                "boom (Ord)"
            );
            self.0.cmp(&o.0)
        }
    }
    static LIVE_KEYS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    static PANIC_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    #[test]
    fn test_insert_no_leak_when_comparator_panics() {
        // Leak-on-panic regression: `insert` allocates the node, then runs user `Eq`/`Ord`
        // during the position search. A panic there must FREE the unpublished node
        // (the `UnpublishedNode` guard frees on unwind), not leak it. A balanced
        // live count proves no leak. The comparator is disarmed before the list drops
        // so teardown does not re-panic.
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::atomic::Ordering::SeqCst;

        {
            let list: SortedList<PanickingKey, DeferredGuard> = SortedList::new();
            assert!(list.insert(PanickingKey::new(1))); // seed so the search actually compares
            let before = LIVE_KEYS.load(SeqCst);

            PANIC_ARMED.store(true, SeqCst);
            let r = catch_unwind(AssertUnwindSafe(|| list.insert(PanickingKey::new(2))));
            PANIC_ARMED.store(false, SeqCst);

            assert!(r.is_err(), "the comparator should have panicked");
            assert_eq!(
                LIVE_KEYS.load(SeqCst),
                before,
                "insert leaked the unpublished node when the comparator panicked"
            );
        } // list drops here (disarmed) — frees the seeded node

        assert_eq!(
            LIVE_KEYS.load(SeqCst),
            0,
            "list drop should free every node"
        );
    }

    #[test]
    fn test_range_start_below_minimum_does_not_panic() {
        // Below-minimum-range regression: `range(start..)` with `start` below the minimum
        // element makes `find_from_internal(.., is_match = false)` return the
        // HEAD-sentinel predecessor position; `next_node_internal` then read its
        // (absent) key for the same-key-replacement skip and panicked.
        let list: SortedList<i32, DeferredGuard> = SortedList::new();
        list.insert(10);
        list.insert(20);
        let v: Vec<i32> = list.range(5..15).map(|x| *x).collect();
        assert_eq!(v, vec![10]);
        let all: Vec<i32> = list.range(0..).map(|x| *x).collect();
        assert_eq!(all, vec![10, 20]);
    }

    #[test]
    fn test_recovery_from_marked_start() {
        let list: SortedList<i32, DeferredGuard> = SortedList::new();

        // Insert nodes 0..100
        for i in 0..100 {
            list.insert(i);
        }

        // Get a pointer to node 50
        DeferredGuard::pin();
        // SAFETY: test uses DeferredGuard — all frees deferred to collection drop; single-threaded.
        let node_50 = unsafe { list.find_from_internal(None, &50, true) }.unwrap();

        // Mark node 50 for deletion - use remove_from_internal to get pointer for cleanup
        // SAFETY: test uses DeferredGuard — all frees deferred to collection drop; single-threaded.
        let deleted_node = unsafe { list.remove_from_internal(None, &50) };
        assert!(deleted_node.is_some());

        // Now try to search starting from the marked node 50
        // Should restart from start_node and find node 60
        let location = list.node_location_from_internal(&60, Some(node_50.node_ptr()));

        // Should successfully find node 60
        assert!(!location.curr.is_null());
        // SAFETY: `location.curr` is non-null (asserted above) and returned by
        // `node_location_from_internal`. The guard is held and the list is alive.
        // `pos.node_ptr()` is the removed node — we own it and can safely deallocate.
        unsafe {
            assert_eq!(*(*location.curr).key(), 60);

            // Clean up deleted node to avoid memory leak
            if let Some(pos) = deleted_node {
                CollectionNode::dealloc_ptr(pos.node_ptr());
            }
        }

        println!("Successfully recovered from marked starting node");
    }

    #[test]
    fn test_marked_pointer_as_start_node() {
        let list: SortedList<i32, DeferredGuard> = SortedList::new();

        // Insert nodes
        for i in 0..100 {
            list.insert(i);
        }

        // Get pointer to node 50
        DeferredGuard::pin();
        // SAFETY: test uses DeferredGuard — all frees deferred to collection drop; single-threaded.
        let node_50 = unsafe { list.find_from_internal(None, &50, true) }.unwrap();

        // Mark node 50 for deletion - use remove_from_internal to get pointer for cleanup
        // SAFETY: test uses DeferredGuard — all frees deferred to collection drop; single-threaded.
        let deleted_node = unsafe { list.remove_from_internal(None, &50) };
        assert!(deleted_node.is_some());

        // The node_50 pointer might now be marked (or point to marked node)
        // This should NOT crash - the code should unmask before dereferencing
        let location = list.node_location_from_internal(&60, Some(node_50.node_ptr()));

        // Should successfully find node 60
        assert!(!location.curr.is_null());
        // SAFETY: `location.curr` is non-null (asserted above) and returned by
        // `node_location_from_internal`. The guard is held and the list is alive.
        // `pos.node_ptr()` is the removed node — we own it and can safely deallocate.
        unsafe {
            assert_eq!(*(*location.curr).key(), 60);

            // Clean up deleted node to avoid memory leak
            if let Some(pos) = deleted_node {
                CollectionNode::dealloc_ptr(pos.node_ptr());
            }
        }

        println!("Successfully handled marked pointer as start_node");
    }

    #[test]
    fn test_concurrent_recovery() {
        let list: Arc<SortedList<i32, DeferredGuard>> = Arc::new(SortedList::new());

        // Pre-populate with nodes
        for i in 0..1000 {
            list.insert(i);
        }

        let num_threads = 8;
        let handles: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let list = Arc::clone(&list);
                thread::spawn(move || {
                    for i in 0..100 {
                        let key = thread_id * 100 + i;

                        // Some threads delete
                        if thread_id % 2 == 0 {
                            list.delete(&key);
                        }

                        // Some threads search (might hit marked nodes)
                        if thread_id % 2 == 1 {
                            let _ = list.contains(&key);
                        }

                        // All threads do some insertions
                        list.insert(key + 1000);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        println!("Concurrent operations with recovery completed successfully");
    }

    #[test]
    fn test_concurrent_delete_insert() {
        let list: Arc<SortedList<i32, DeferredGuard>> = Arc::new(SortedList::new());
        let num_threads = 4;
        let operations_per_thread = 100;

        let handles: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let list = Arc::clone(&list);
                thread::spawn(move || {
                    for i in 0..operations_per_thread {
                        let key = thread_id * operations_per_thread + i;
                        list.insert(key);

                        // Occasionally delete
                        if i % 10 == 0 && key > 0 {
                            list.delete(&(key - 1));
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        println!("Concurrent delete/insert operations completed successfully");
    }

    // =========================================================================
    // Update operation tests (UPDATE_MARK)
    // =========================================================================

    #[test]
    fn test_update_basic() {
        let list: SortedList<i32, DeferredGuard> = SortedList::new();

        // Insert initial values
        list.insert(10);
        list.insert(20);
        list.insert(30);

        // Verify initial state
        assert!(list.contains(&10));
        assert!(list.contains(&20));
        assert!(list.contains(&30));

        // Update value 20 -> 20 (same key, could be different payload in real use)
        let result = list.update(20);
        assert!(result);

        // Value should still be findable
        assert!(list.contains(&20));

        // List should still contain all values
        let values = list.to_vec();
        assert_eq!(values.len(), 3);
        assert!(values.contains(&10));
        assert!(values.contains(&20));
        assert!(values.contains(&30));

        println!("Basic update test passed");
    }

    #[test]
    fn test_update_not_found() {
        let list: SortedList<i32, DeferredGuard> = SortedList::new();

        list.insert(10);
        list.insert(30);

        // Try to update non-existent key
        let result = list.update(20);
        assert!(!result);

        // List unchanged
        let values = list.to_vec();
        assert_eq!(values.len(), 2);

        println!("Update not found test passed");
    }

    #[test]
    fn test_update_preserves_order() {
        let list: SortedList<i32, DeferredGuard> = SortedList::new();

        // Insert in order
        for i in 0..10 {
            list.insert(i);
        }

        // Update middle value
        let result = list.update(5);
        assert!(result);

        // Verify order preserved
        let values = list.to_vec();
        assert_eq!(values.len(), 10);
        assert_eq!(values, (0..10).collect::<Vec<_>>());

        println!("Update preserves order test passed");
    }

    #[test]
    fn test_concurrent_updates() {
        let list: Arc<SortedList<i32, DeferredGuard>> = Arc::new(SortedList::new());
        let num_elements = 100;

        // Insert initial elements
        for i in 0..num_elements {
            list.insert(i);
        }

        let num_threads = 4;
        let updates_per_thread = 50;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let list = Arc::clone(&list);
                thread::spawn(move || {
                    for _ in 0..updates_per_thread {
                        // Update random elements
                        for j in 0..10 {
                            let key = j * 10;
                            list.update(key);
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // All elements should still exist
        for i in 0..num_elements {
            assert!(list.contains(&i));
        }

        println!("Concurrent updates test passed");
    }

    #[test]
    fn test_update_with_concurrent_delete() {
        let list: Arc<SortedList<i32, DeferredGuard>> = Arc::new(SortedList::new());

        // Insert elements
        for i in 0..100 {
            list.insert(i);
        }

        let list_update = Arc::clone(&list);
        let list_delete = Arc::clone(&list);

        // One thread updates
        let update_handle = thread::spawn(move || {
            for _ in 0..100 {
                for j in (0..100).step_by(2) {
                    list_update.update(j);
                }
            }
        });

        // Another thread deletes
        let delete_handle = thread::spawn(move || {
            for _ in 0..100 {
                for j in (1..100).step_by(2) {
                    list_delete.delete(&j);
                }
            }
        });

        update_handle.join().unwrap();
        delete_handle.join().unwrap();

        println!("Update with concurrent delete test passed");
    }

    #[test]
    fn test_update_with_concurrent_insert() {
        let list: Arc<SortedList<i32, DeferredGuard>> = Arc::new(SortedList::new());

        // Insert initial elements (even numbers)
        for i in (0..100).step_by(2) {
            list.insert(i);
        }

        let list_update = Arc::clone(&list);
        let list_insert = Arc::clone(&list);

        // One thread updates
        let update_handle = thread::spawn(move || {
            for _ in 0..100 {
                for j in (0..100).step_by(2) {
                    list_update.update(j);
                }
            }
        });

        // Another thread inserts (odd numbers)
        let insert_handle = thread::spawn(move || {
            for j in (1..100).step_by(2) {
                list_insert.insert(j);
            }
        });

        update_handle.join().unwrap();
        insert_handle.join().unwrap();

        // All numbers 0-99 should exist
        for i in 0..100 {
            assert!(list.contains(&i), "Missing key: {}", i);
        }

        println!("Update with concurrent insert test passed");
    }
}
