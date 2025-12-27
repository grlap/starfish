use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::data_structures::CollectionNode;
use crate::data_structures::MarkedPtr;
use crate::data_structures::NodePosition;
use crate::data_structures::SortedCollection;
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
//  ToDO:
//  - [x] find -> SortedListNodeLocator
//  - [x] implement as SortedCollection trait
//  - [x] delete -> use remove_from_internal
//  - [ ] remove +Clone trait for the key
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

    /// Load next pointer (Acquire ordering)
    #[inline]
    pub(super) fn get_next(&self) -> NodePtr<T> {
        self.next.load(Ordering::Acquire)
    }

    /// Store next pointer (Release ordering)
    #[inline]
    fn set_next(&self, ptr: NodePtr<T>) {
        self.next.store(ptr, Ordering::Release)
    }

    /// CAS next pointer (Release/Relaxed ordering)
    #[inline]
    fn cas_next(&self, expected: NodePtr<T>, new: NodePtr<T>) -> Result<NodePtr<T>, NodePtr<T>> {
        self.next
            .compare_exchange(expected, new, Ordering::Release, Ordering::Relaxed)
    }

    /// Weak CAS next pointer (Release/Relaxed ordering)
    #[inline]
    fn cas_next_weak(
        &self,
        expected: NodePtr<T>,
        new: NodePtr<T>,
    ) -> Result<NodePtr<T>, NodePtr<T>> {
        self.next
            .compare_exchange_weak(expected, new, Ordering::Release, Ordering::Relaxed)
    }
}

impl<T> CollectionNode<T> for SortedListNode<T> {
    fn key(&self) -> &T {
        self.data
            .as_ref()
            .expect("Cannot get key from sentinel node")
    }
}

// Represents a node location in a sorted linked list.
//
#[derive(Debug, Copy, Clone)]
struct NodeLocation<T> {
    pub pred: NodePtr<T>,
    pub curr: NodePtr<T>,
    pub next: NodePtr<T>,
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

// Manual impls to avoid requiring T: Clone/Copy
impl<T> Copy for ListNodePosition<T> {}

impl<T> Clone for ListNodePosition<T> {
    fn clone(&self) -> Self {
        *self
    }
}

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
        let key = unsafe { (*marked_node).key() };
        // Shadow parameter with mutable local - once invalid, stays None
        let mut start_node = start_node;

        loop {
            // Try to unlink: CAS pred.next from marked_node to replacement
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
            // with key >= marked_node's key, the node was already unlinked
            if actual_ptr != marked_node {
                if actual_ptr.is_null() {
                    // pred.next is null - marked_node was definitely unlinked
                    return pred;
                }
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

                let next = unsafe { (*curr).get_next() };
                let next_marked = MarkedPtr::new(next);

                // Skip over any marked nodes during traversal
                if next_marked.is_any_marked() {
                    // Try to help snip this marked node
                    let snip_result = unsafe { (*pred).cas_next(curr, next_marked.as_ptr()) };

                    if snip_result.is_err() {
                        // Snip failed - pred might be marked (being deleted by another thread)
                        // Check if pred.next is marked, indicating pred is being removed
                        let pred_next_raw = unsafe { (*pred).get_next() };
                        if MarkedPtr::new(pred_next_raw).is_any_marked() {
                            // pred is being deleted, need to restart from a valid node
                            // Try start first (more efficient), fall back to HEAD if start is also marked
                            let start_next = unsafe { (*start).get_next() };
                            if MarkedPtr::new(start_next).is_any_marked() {
                                // start is also invalid, update it to HEAD for all future restarts
                                // Also invalidate start_node for future outer loop iterations
                                start_node = None;
                                start = self.head.load(Ordering::Acquire);
                            }
                            pred = start;
                            curr = unsafe { (*pred).get_next() };
                            continue;
                        }
                    }

                    curr = unsafe { (*pred).get_next() };
                    continue;
                }

                // Check if we've passed marked_node's position
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
    fn node_location_from_internal(
        &self,
        key: &T,
        head_node: Option<NodePtr<T>>,
    ) -> NodeLocation<T> {
        'retry: loop {
            let mut pred_node = match head_node {
                Some(start_node) => MarkedPtr::unmask(start_node),
                None => self.head.load(Ordering::Acquire),
            };

            let mut curr_node = unsafe { (*pred_node).get_next() };

            loop {
                curr_node = MarkedPtr::unmask(curr_node);

                if curr_node.is_null() {
                    return NodeLocation {
                        pred: pred_node,
                        curr: curr_node,
                        next: ptr::null_mut(),
                    };
                }

                let next_node = unsafe { (*curr_node).get_next() };
                let next_marked = MarkedPtr::new(next_node);

                if next_marked.is_any_marked() {
                    // Try to physically remove the marked node (DELETE or UPDATE).
                    // For both: snip curr out by pointing pred to curr's successor.
                    // For UPDATE-marked: next_marked.as_ptr() is the replacement node.
                    //
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
                    unsafe {
                        if !(*curr_node).is_sentinel() && (*curr_node).key() >= key {
                            // Before returning, double-check the node isn't marked.
                            //
                            let recheck = (*curr_node).get_next();
                            if MarkedPtr::new(recheck).is_any_marked() {
                                // Node got marked while we were checking - restart
                                continue 'retry;
                            }
                            return NodeLocation {
                                pred: pred_node,
                                curr: curr_node,
                                next: next_node,
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

/// Implement SortedCollection for SortedList.
///
impl<T, G> SortedCollection<T> for SortedList<T, G>
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
    fn insert_from_internal(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition> {
        let new_node = Box::into_raw(Box::new(SortedListNode::new(key)));

        loop {
            let key = unsafe { (*new_node).key() };

            // Extract the predecessor from position if provided
            let start_node = position.and_then(|pos| {
                // Use predecessor if available, otherwise fall back to node
                let pred = pos.pred();
                if !pred.is_null() {
                    Some(pred)
                } else {
                    pos.node()
                }
            });

            // Check if start_node itself has the same key (duplicate check for hint-based insert).
            // This prevents inserting duplicates when the hint node IS the duplicate.
            //
            if let Some(hint) = start_node {
                let hint = MarkedPtr::unmask(hint);
                unsafe {
                    if !(*hint).is_sentinel() && (*hint).key() == key {
                        // Clean up unused node.
                        //
                        SortedListNode::dealloc_ptr(new_node);
                        return None; // Duplicate - hint node has the same key
                    }
                }
            }

            let loc = self.node_location_from_internal(key, start_node);

            let (pred, curr) = (loc.pred, loc.curr);

            // Check for duplicate.
            //
            if !curr.is_null() {
                unsafe {
                    if (*curr).key() == key {
                        // Clean up unused node.
                        //
                        SortedListNode::dealloc_ptr(new_node);
                        return None; // Duplicate
                    }
                }
            }

            // Set new node's next pointer
            // Backlink stays null - only set on delete
            unsafe {
                (*new_node).set_next(curr);
            }

            // Try to link new node
            let result = unsafe { (*pred).cas_next_weak(curr, new_node) };

            if result.is_ok() {
                // Return position with predecessor for O(1) batch operations
                return Some(ListNodePosition::new(pred, new_node));
            }
            // CAS failed, retry
        }
    }

    /// Removes a node starting from a position and return position if it existed.
    ///
    fn remove_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
    ) -> Option<Self::NodePosition> {
        // Extract the start node from position
        let start_node = position.and_then(|pos| {
            let pred = pos.pred();
            if !pred.is_null() {
                Some(pred)
            } else {
                pos.node()
            }
        });

        loop {
            let location = self.node_location_from_internal(key, start_node);
            let (mut pred, curr) = (location.pred, location.curr);

            if curr.is_null() {
                return None;
            }

            unsafe {
                // Verify key matches
                //
                let key_match = (*curr).key() == key;

                if !key_match {
                    return None;
                }

                pred = MarkedPtr::unmask(pred);

                // Load the current next pointer.
                //
                let curr_next = (*curr).get_next();
                let curr_next_marked = MarkedPtr::new(curr_next);

                // Check if already marked (DELETE or UPDATE).
                // If UPDATE-marked, someone else is updating it - we should retry
                // to find the replacement node. If DELETE-marked, already deleted.
                //
                if curr_next_marked.is_any_marked() {
                    if curr_next_marked.is_marked() {
                        // DELETE-marked: someone else deleted it
                        return None;
                    }
                    // UPDATE-marked: retry to find the replacement node
                    continue;
                }

                // Try to mark for deletion.
                //
                let marked = curr_next_marked.with_mark(true);
                let mark_result = (*curr).cas_next_weak(curr_next, marked.as_raw());

                if mark_result.is_err() {
                    // Someone marked the node or updated the next pointer.
                    //
                    continue;
                }

                // Successfully marked! Unlink the node - this MUST complete before return.
                // Required for safe epoch-based reclamation.
                let successor = curr_next_marked.as_ptr();
                let final_pred = self.unlink_marked_node(pred, curr, successor, start_node);
                return Some(ListNodePosition::new(final_pred, curr));
            }
        }
    }

    fn find_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
        is_match: bool,
    ) -> Option<Self::NodePosition> {
        // Extract the start node from position
        let start_node = position.and_then(|pos| {
            let pred = pos.pred();
            if !pred.is_null() {
                Some(pred)
            } else {
                pos.node()
            }
        });

        let location = self.node_location_from_internal(key, start_node);

        if location.curr.is_null() {
            return None;
        }

        let node = location.curr;

        // Check the key (no sentinel check needed).
        //
        if is_match {
            if unsafe { (*node).key() == key } {
                return Some(ListNodePosition::new(location.pred, node));
            } else {
                return None;
            }
        }

        Some(ListNodePosition::new(location.pred, location.pred))
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
    fn update_internal(
        &self,
        position: Option<&Self::NodePosition>,
        new_value: T,
    ) -> Option<(*mut Self::Node, Self::NodePosition)> {
        let new_node = Box::into_raw(Box::new(SortedListNode::new(new_value)));

        // Use the new node's key for lookups
        let key = unsafe { (*new_node).key() };

        loop {
            // Extract the start node from position
            let start_node = position.and_then(|pos| {
                let pred = pos.pred();
                if !pred.is_null() {
                    Some(pred)
                } else {
                    pos.node()
                }
            });

            let location = self.node_location_from_internal(key, start_node);
            let (mut pred, curr) = (location.pred, location.curr);

            // Key not found
            if curr.is_null() {
                unsafe {
                    SortedListNode::dealloc_ptr(new_node);
                }
                return None;
            }

            unsafe {
                // Verify key matches
                if (*curr).key() != key {
                    SortedListNode::dealloc_ptr(new_node);
                    return None;
                }

                // Get curr's next pointer (the successor)
                let curr_next = (*curr).get_next();
                let curr_next_marked = MarkedPtr::new(curr_next);

                // If curr is already DELETE-marked, someone else is deleting it
                if curr_next_marked.is_marked() {
                    continue; // Retry - will find a different node or none
                }

                // If curr is already UPDATE-marked, another update is in progress
                // In forward insertion approach, curr.next already points to new value
                if curr_next_marked.is_update_marked() {
                    continue; // Retry - the key might be gone or we'll find the new node
                }

                // Set up new_node: point to curr's successor
                // After CAS: curr -> new_node -> succ
                let succ = curr_next_marked.as_ptr();
                (*new_node).set_next(succ);

                pred = MarkedPtr::unmask(pred);

                // LINEARIZATION POINT: CAS curr.next from succ to (new_node | UPDATE_MARK)
                // This atomically:
                // 1. Inserts new_node right after curr (curr -> new_node -> succ)
                // 2. Marks curr as UPDATE-marked
                // Readers finding UPDATE-marked curr will follow curr.next to find new_node
                let update_marked_new_node =
                    MarkedPtr::new(new_node).with_update_mark(true).as_raw();
                let mark_result = (*curr).cas_next_weak(curr_next, update_marked_new_node);

                if mark_result.is_err() {
                    // Someone modified curr.next, retry
                    continue;
                }

                // Successfully UPDATE-marked with new_node inserted!
                // The UPDATE is logically complete (linearization point was the CAS above).
                // Now unlink curr - this MUST complete before return.
                // Required for safe epoch-based reclamation.
                let final_pred = self.unlink_marked_node(pred, curr, new_node, start_node);
                return Some((curr, ListNodePosition::new(final_pred, new_node)));
            }
        }
    }

    /// Apply a function to the given node's value.
    ///
    /// If we have a pointer to a node, use it directly - no need to follow UPDATE marks.
    /// UPDATE-mark following is only needed during traversal to find a node by key.
    fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        let curr = MarkedPtr::unmask(node);

        if curr.is_null() {
            return None;
        }

        unsafe {
            let node_ref = &*curr;

            if node_ref.is_sentinel() {
                return None;
            }

            Some(f(node_ref.key()))
        }
    }

    fn first_node_internal(&self) -> Option<*mut Self::Node> {
        let head = self.head.load(Ordering::Acquire);
        let mut curr = unsafe { (*head).get_next() };

        while !curr.is_null() {
            let marked = MarkedPtr::new(unsafe { (*curr).get_next() });

            // Skip both DELETE-marked and UPDATE-marked nodes
            if !marked.is_any_marked() {
                return Some(curr);
            }

            curr = marked.as_ptr();
        }

        None
    }

    fn next_node_internal(&self, node: *mut Self::Node) -> Option<*mut Self::Node> {
        if node.is_null() {
            return None;
        }

        let node = MarkedPtr::unmask(node);

        unsafe {
            // Get next pointer, skip marked nodes
            let mut curr = (*node).get_next();

            while !curr.is_null() {
                let marked = MarkedPtr::new(curr);
                curr = marked.as_ptr();

                if curr.is_null() {
                    return None;
                }

                // Check if this node is marked (DELETE or UPDATE)
                let next_marked = MarkedPtr::new((*curr).get_next());
                if !next_marked.is_any_marked() {
                    return Some(curr);
                }

                // Node is marked, continue to next
                curr = next_marked.as_ptr();
            }
        }

        None
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
            unsafe {
                let next_raw = (*curr).get_next();
                let next_marked = MarkedPtr::new(next_raw);

                // Check invariant: no DELETE-marked nodes should exist at drop time.
                // DELETE-marked nodes indicate incomplete physical unlinking.
                // UPDATE-marked nodes are allowed as they'll be cleaned up normally.
                if next_marked.is_marked() && !(*curr).is_sentinel() {
                    panic!(
                        "INVARIANT VIOLATION: Found DELETE-marked node at drop time!\n\
                         DELETE-marked nodes should have been physically unlinked before drop."
                    );
                }

                let next = next_marked.as_ptr();
                SortedListNode::dealloc_ptr(curr);

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

    #[test]
    fn test_recovery_from_marked_start() {
        let list: SortedList<i32, DeferredGuard> = SortedList::new();

        // Insert nodes 0..100
        for i in 0..100 {
            list.insert(i);
        }

        // Get a pointer to node 50
        let _guard = DeferredGuard::pin();
        let node_50 = list.find_from_internal(None, &50, true).unwrap();

        // Mark node 50 for deletion - use remove_from_internal to get pointer for cleanup
        let deleted_node = list.remove_from_internal(None, &50);
        assert!(deleted_node.is_some());

        // Now try to search starting from the marked node 50
        // Should restart from start_node and find node 60
        let location = list.node_location_from_internal(&60, Some(node_50.node_ptr()));

        // Should successfully find node 60
        assert!(!location.curr.is_null());
        unsafe {
            assert_eq!(*(*location.curr).key(), 60);

            // Clean up deleted node to avoid memory leak
            if let Some(pos) = deleted_node {
                SortedListNode::dealloc_ptr(pos.node_ptr());
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
        let _guard = DeferredGuard::pin();
        let node_50 = list.find_from_internal(None, &50, true).unwrap();

        // Mark node 50 for deletion - use remove_from_internal to get pointer for cleanup
        let deleted_node = list.remove_from_internal(None, &50);
        assert!(deleted_node.is_some());

        // The node_50 pointer might now be marked (or point to marked node)
        // This should NOT crash - the code should unmask before dereferencing
        let location = list.node_location_from_internal(&60, Some(node_50.node_ptr()));

        // Should successfully find node 60
        assert!(!location.curr.is_null());
        unsafe {
            assert_eq!(*(*location.curr).key(), 60);

            // Clean up deleted node to avoid memory leak
            if let Some(pos) = deleted_node {
                SortedListNode::dealloc_ptr(pos.node_ptr());
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
        for i in 0..10 {
            assert_eq!(values[i], i as i32);
        }

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
