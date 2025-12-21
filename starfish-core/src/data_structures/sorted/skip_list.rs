use std::alloc::{Layout, alloc, dealloc};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::data_structures::{CollectionNode, MarkedPtr, NodePosition, SortedCollection};

const MAX_LEVEL: usize = 16;
const PROBABILITY: f64 = 0.5;

type SkipNodePtr<T> = *mut SkipNode<T>;

// =============================================================================
// SKIP LIST INVARIANTS & REMOVE OPERATION
// =============================================================================
//
// IMPORTANT: Lock-free algorithms must be correct by design. When bugs occur
// (hangs, crashes, corruption), fix the ROOT CAUSE - do not add defensive
// checks or hacks to work around symptoms. Adding workarounds hides bugs and
// makes the code harder to reason about. Every operation must work correctly
// on its own merits.
//
// Skip List Structure (sorted ascending, multiple levels):
//
// Level 3:  HEAD ─────────────────────────────────────► 30 ─────────────────► NULL
//             │                                          │
// Level 2:  HEAD ──────────► 10 ─────────────────────► 30 ─────────────────► NULL
//             │               │                          │
// Level 1:  HEAD ──────────► 10 ──────────► 20 ──────► 30 ─────────────────► NULL
//             │               │              │           │
// Level 0:  HEAD ──────────► 10 ──────────► 20 ──────► 30 ──────────► 40 ──► NULL
//
// Marked Pointer: The mark bit on node.next[level] indicates the NODE is logically
//                 deleted at that level. Marking happens bottom-up (level 0 first).
//
// INVARIANTS:
// 1. List is always sorted by key (ascending) at all levels
// 2. No duplicate keys allowed
// 3. Marked nodes must be physically unlinked at ALL levels before Drop
// 4. HEAD sentinel is never marked or removed
// 5. Only the thread that marks level 0 "owns" the node and must unlink it
//
// =============================================================================
// REMOVE OPERATION (Two-Phase Delete, Per-Level)
// =============================================================================
//
// Phase 1: LOGICAL DELETE (mark node.next at each level, bottom-up)
// Phase 2: PHYSICAL UNLINK (CAS pred.next from node to node.next, top-down)
//
// Normal Remove at Level 0 (no contention):
// ─────────────────────────────────────────
// Before:  pred ──────► node ──────► next
//
// Step 1 - Mark node (logical delete):
//          pred ──────► node ──╳───► next
//                              │
//                           (marked)
//
// Step 2 - Unlink (CAS pred.next from node to next):
//          pred ─────────────────────► next
//                       node ──╳───► next  (unlinked)
//
// =============================================================================
// CAS FAILURE CASES IN PHYSICAL UNLINK (same logic at each level)
// =============================================================================
//
// When CAS(pred.next[level], node, next) fails, we get actual = pred.next[level]
//
// CASE 1: actual.as_ptr() == node (but CAS failed)
// ─────────────────────────────────────────────────
// Meaning: pred is marked (actual = node | MARK)
// Solution: Retry with find_at_level to get fresh predecessor
//
// CASE 2: actual.as_ptr() != node AND actual.key >= node.key
// ───────────────────────────────────────────────────────────
// Meaning: node was already unlinked at this level by another thread
// Solution: Break, move to next level
//
// CASE 3: actual.as_ptr() != node AND actual.key < node.key
// ──────────────────────────────────────────────────────────
// Meaning: A node was INSERTED between pred and node at this level
//
// Before:  pred ──────► node(20) ──╳───► next
//
// After:   pred ──────► X(15) ──────► node(20) ──╳───► next
//                       ▲
//                    actual (inserted node)
//
// actual.key (15) < node.key (20) → insert happened!
// Solution: Retry with find_at_level to advance pred past inserted node
//
// =============================================================================
// RECOVERY STRATEGY (using preds[level+1])
// =============================================================================
//
// When traversing and encountering a marked predecessor, we use the predecessors
// array from higher levels to recover, instead of restarting from HEAD.
//
// KEY INSIGHT: During find_position, we traverse top-down, recording predecessors
// at each level. If preds[level] becomes invalid (marked), we can use preds[level+1]
// which was valid at a higher level and thus is a safe starting point.
//
// RECOVERY ALGORITHM (recover_pred):
// ─────────────────────────────────────────────────────────────────────────────
//   1. Check preds[level+1], preds[level+2], etc. in order
//   2. For each pred, verify it's not logically deleted (check next[0] mark)
//   3. Verify pred has sufficient height for the current level
//   4. Return first valid pred, or HEAD as fallback
//
// COMPLEXITY:
//   - Expected O(1) - typically only need to go up one level
//   - Worst case O(log n) if many levels have stale preds
//
// VARIABLE HEIGHT NODES:
// ─────────────────────────────────────────────────────────────────────────────
// Each node has height H and only has next[0..H].
// When traversing level L, we can only use nodes with height > L.
//
// Example: To delete node 30 (height=4), we need preds at each level:
//
//   Level 3:  HEAD ─────────────────────────────────────► 30 ──► NULL
//   Level 2:  HEAD ──────────► 10 ─────────────────────► 30 ──► NULL
//   Level 1:  HEAD ──────────► 10 ──────────► 20 ──────► 30 ──► NULL
//   Level 0:  HEAD ──────────► 10 ──────────► 20 ──────► 30 ──► 40 ──► NULL
//
//   preds = [20, 20, 10, HEAD]  (each pred[L] only needs links at level L!)
//
// TRAVERSAL ALGORITHM (find_position):
// ─────────────────────────────────────────────────────────────────────────────
// Start from HEAD at max_level-1, descend level by level:
//   - At each level L, call find_at_level(key, L, pred_from_level_above)
//   - pred_from_level_above is always valid at level L (HEAD has all levels)
//   - The returned pred becomes the starting point for level L-1
//
// This gives O(log n) overall because:
//   - We traverse ~log(n) levels
//   - At each level, we skip ~n/2^level nodes
//
// =============================================================================

// ============================================================================
// SkipNode - Multi-level node with forward pointers
// ============================================================================

/// A skip list node with tower structure.
///
/// Uses flexible array member pattern for efficient memory layout:
/// - Single allocation per node (no separate heap allocations for pointers)
/// - Pointers are inline after the struct fields
/// - Layout: [next[0..h]] where h = height
///
/// Each node has:
/// - Key/value data (None for sentinel)
/// - Forward pointers (next) at each level, with mark bits for deletion
///
#[repr(C)]
pub struct SkipNode<T> {
    value: Option<T>,
    height: usize,
    // Flexible array: pointers are allocated inline after this struct
    // Layout: [next[0], next[1], ..., next[h-1]]
    // Total: height pointers
    pointers: [AtomicPtr<SkipNode<T>>; 0],
}

impl<T> SkipNode<T> {
    /// Calculate layout for a node with given height
    fn get_layout(height: usize) -> Layout {
        Layout::new::<Self>()
            .extend(Layout::array::<AtomicPtr<Self>>(height).unwrap())
            .unwrap()
            .0
            .pad_to_align()
    }

    /// Allocate and initialize a new node with a key
    fn alloc_with_key(key: T, height: usize) -> *mut Self {
        unsafe {
            let layout = Self::get_layout(height);
            let ptr = alloc(layout) as *mut Self;
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }

            // Initialize fields
            ptr::write(&mut (*ptr).value, Some(key));
            ptr::write(&mut (*ptr).height, height);

            // Initialize all pointers to null
            let pointers_base = (*ptr).pointers.as_ptr() as *mut AtomicPtr<Self>;
            for i in 0..height {
                ptr::write(pointers_base.add(i), AtomicPtr::new(ptr::null_mut()));
            }

            ptr
        }
    }

    /// Allocate and initialize a sentinel node (no value)
    fn alloc_sentinel(height: usize) -> *mut Self {
        unsafe {
            let layout = Self::get_layout(height);
            let ptr = alloc(layout) as *mut Self;
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }

            // Initialize fields - sentinel has no value
            ptr::write(&mut (*ptr).value, None);
            ptr::write(&mut (*ptr).height, height);

            // Initialize all pointers to null
            let pointers_base = (*ptr).pointers.as_ptr() as *mut AtomicPtr<Self>;
            for i in 0..height {
                ptr::write(pointers_base.add(i), AtomicPtr::new(ptr::null_mut()));
            }

            ptr
        }
    }

    /// Deallocate a node
    ///
    /// # Safety
    /// The pointer must have been allocated by alloc_with_key or alloc_sentinel
    unsafe fn dealloc_node(ptr: *mut Self) {
        unsafe {
            let height = (*ptr).height;
            let layout = Self::get_layout(height);

            // Drop the value if present (Option<T> handles this automatically)
            ptr::drop_in_place(&mut (*ptr).value);

            dealloc(ptr as *mut u8, layout);
        }
    }

    #[inline]
    fn is_sentinel(&self) -> bool {
        self.value.is_none()
    }

    /// Get height
    #[inline]
    fn height(&self) -> usize {
        self.height
    }

    /// Take ownership of the value from this node.
    ///
    /// # Safety
    /// - Must only be called on an unlinked node (never inserted into list)
    /// - Must only be called once
    /// - Node must contain an initialized value
    ///
    /// This is an internal method used for CAS retry recovery, NOT exposed
    /// through the CollectionNode trait (which would be unsafe in concurrent contexts).
    unsafe fn take_value_unlinked(&mut self) -> T {
        self.value.take().expect("Cannot take value from sentinel")
    }

    // =========================================================================
    // Pointer access helpers
    // =========================================================================

    /// Get pointer to the AtomicPtr at given index in the flexible array
    #[inline]
    unsafe fn pointer_at(&self, index: usize) -> &AtomicPtr<SkipNode<T>> {
        unsafe { &*self.pointers.as_ptr().add(index) }
    }

    // =========================================================================
    // Next pointer accessors (indices 0..height)
    // =========================================================================

    /// Load next pointer at level (Acquire ordering)
    #[inline]
    fn get_next(&self, level: usize) -> *mut SkipNode<T> {
        unsafe { self.pointer_at(level).load(Ordering::Acquire) }
    }

    /// Store next pointer at level (Release ordering)
    #[inline]
    fn set_next(&self, level: usize, ptr: *mut SkipNode<T>) {
        unsafe { self.pointer_at(level).store(ptr, Ordering::Release) }
    }

    /// CAS next pointer at level (Release/Relaxed ordering)
    #[inline]
    fn cas_next(
        &self,
        level: usize,
        expected: *mut SkipNode<T>,
        new: *mut SkipNode<T>,
    ) -> Result<*mut SkipNode<T>, *mut SkipNode<T>> {
        unsafe {
            self.pointer_at(level).compare_exchange(
                expected,
                new,
                Ordering::Release,
                Ordering::Relaxed,
            )
        }
    }

    /// Weak CAS next pointer at level (Release/Relaxed ordering)
    #[inline]
    fn cas_next_weak(
        &self,
        level: usize,
        expected: *mut SkipNode<T>,
        new: *mut SkipNode<T>,
    ) -> Result<*mut SkipNode<T>, *mut SkipNode<T>> {
        unsafe {
            self.pointer_at(level).compare_exchange_weak(
                expected,
                new,
                Ordering::Release,
                Ordering::Relaxed,
            )
        }
    }
}

impl<T> CollectionNode<T> for SkipNode<T> {
    #[inline]
    fn key(&self) -> &T {
        self.value
            .as_ref()
            .expect("Cannot get key from sentinel node")
    }

    /// Deallocate using custom allocator (flexible array member pattern).
    ///
    /// SkipNode uses a custom layout with the allocator API for its flexible
    /// array member (pointers), so we must use the matching deallocation.
    ///
    unsafe fn dealloc_ptr(ptr: *mut Self) {
        unsafe {
            Self::dealloc_node(ptr);
        }
    }
}

// ============================================================================
// SkipNodePosition - Position with predecessors at all levels
// ============================================================================

/// Position in a SkipList containing predecessors at all levels and current node.
///
/// This enables O(1) amortized batch inserts for sorted data because
/// we have predecessors at ALL levels, not just the hint node's height.
///
/// For batch inserts of sorted data, each insert only needs to traverse
/// ~1 node forward per level from the previous position.
///
pub struct SkipNodePosition<T> {
    /// Predecessors at all levels for O(1) batch operations
    preds: [SkipNodePtr<T>; MAX_LEVEL],
    /// Current node at this position
    node: SkipNodePtr<T>,
}

// Manual impls to avoid requiring T: Clone/Copy
impl<T> Copy for SkipNodePosition<T> {}

impl<T> Clone for SkipNodePosition<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> NodePosition<T> for SkipNodePosition<T> {
    type Node = SkipNode<T>;

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
        SkipNodePosition {
            preds: [ptr::null_mut(); MAX_LEVEL],
            node: ptr::null_mut(),
        }
    }

    fn from_node(node: *mut Self::Node) -> Self {
        SkipNodePosition {
            preds: [ptr::null_mut(); MAX_LEVEL],
            node,
        }
    }

    fn is_valid(&self) -> bool {
        !self.node.is_null()
    }
}

impl<T> SkipNodePosition<T> {
    /// Create a new position with predecessors and node
    pub fn new(preds: [SkipNodePtr<T>; MAX_LEVEL], node: SkipNodePtr<T>) -> Self {
        SkipNodePosition { preds, node }
    }

    /// Get the predecessors array
    pub fn preds(&self) -> &[SkipNodePtr<T>; MAX_LEVEL] {
        &self.preds
    }
}

// ============================================================================
// SearchResult - Result of searching at one level
// ============================================================================

struct SearchResult<T> {
    pred: *mut SkipNode<T>,
    curr: *mut SkipNode<T>,
}

// ============================================================================
// SkipList - Lock-free skip list with level-based recovery
// ============================================================================

/// A lock-free skip list with recovery using predecessors from higher levels.
///
/// Structure:
/// - Multi-level linked lists (levels 0 to MAX_LEVEL-1)
/// - Each node has a tower of forward pointers
/// - Uses marked pointers for lock-free deletion
///
/// Recovery strategy:
/// - When a predecessor becomes invalid, use preds[level+1] for recovery
/// - This avoids restarting from HEAD while being memory-safe
///
pub struct SkipList<T> {
    head: *mut SkipNode<T>,
    max_level: usize,
}

impl<T: Ord> SkipList<T> {
    /// Create a new empty skip list
    pub fn new() -> Self {
        let head = SkipNode::alloc_sentinel(MAX_LEVEL);

        SkipList {
            head,
            max_level: MAX_LEVEL,
        }
    }

    /// Generate a random height for a new node
    /// Generate random level using bit manipulation for efficiency.
    ///
    /// Instead of calling RNG multiple times in a loop, we generate a single
    /// random number and count trailing zeros. With PROBABILITY = 0.5:
    /// - Level 1: 50% (bit 0 is 1)
    /// - Level 2: 25% (bits 0-1 are 0, bit 2 is 1)
    /// - Level N: (1/2)^N
    ///
    /// This is ~4x faster than the loop-based approach.
    #[inline]
    fn random_level() -> usize {
        // Generate random bits and count trailing ones
        // Each trailing 1 bit adds one level (with 50% probability each)
        let random_bits = fastrand::u32(..);

        // Count trailing ones (equivalent to counting consecutive "heads" in coin flips)
        // trailing_ones() counts how many 1s before the first 0
        let extra_levels = (!random_bits).trailing_zeros() as usize;

        // Clamp to MAX_LEVEL (level is 1-indexed, so max value is MAX_LEVEL)
        (1 + extra_levels).min(MAX_LEVEL)
    }

    /// Recovery when encountering a marked predecessor.
    ///
    /// Instead of using backlinks (which have memory safety issues),
    /// we restart from the predecessor at level+1.
    ///
    /// IMPORTANT: The preds array might contain stale pointers to nodes that
    /// have been marked by other threads. We must validate and go up multiple
    /// levels if needed.
    ///
    /// A node is logically deleted when its next[0] is DELETE-marked.
    /// We check level 0 to determine if a pred is still valid.
    ///
    #[inline]
    fn recover_pred(&self, level: usize, preds: &[SkipNodePtr<T>]) -> *mut SkipNode<T> {
        // Try each higher level until we find an unmarked pred or reach HEAD
        for &pred in preds.iter().skip(level + 1) {
            if pred.is_null() {
                continue;
            }

            // Skip HEAD - it's always valid, use it directly if we reach here
            if pred == self.head {
                return self.head;
            }

            // Check if this pred is logically deleted (next[0] is marked)
            unsafe {
                let pred_next_0 = (*pred).get_next(0);
                if !MarkedPtr::new(pred_next_0).is_any_marked() {
                    // Pred is not deleted - but verify it has height > level
                    if (*pred).height() > level {
                        return pred;
                    }
                }
            }
            // This pred is deleted or doesn't have the right height, try next level up
        }

        // All preds were null or marked - fall back to HEAD
        self.head
    }

    /// Find a key's position at a specific level
    ///
    /// Returns (predecessor, current) where:
    /// - predecessor.key < search_key
    /// - current.key >= search_key (or null)
    ///
    /// Uses preds[level+1] for recovery when starting node is marked or CAS fails.
    ///
    #[inline]
    fn find_at_level(
        &self,
        key: &T,
        level: usize,
        start: Option<*mut SkipNode<T>>,
        preds: &[SkipNodePtr<T>],
    ) -> SearchResult<T> {
        let mut pred = start.unwrap_or(self.head);

        // Unmask start node defensively
        pred = MarkedPtr::unmask(pred);

        // Check if starting node ITSELF is marked (logically deleted)
        // A node is marked when its next pointer has the mark bit set
        unsafe {
            let pred_next = (*pred).get_next(level);
            let pred_next_marked = MarkedPtr::new(pred_next);
            if pred_next_marked.is_any_marked() {
                // Node is marked - recover from level+1
                pred = self.recover_pred(level, preds);
            }
        }

        let mut curr = unsafe { MarkedPtr::new((*pred).get_next(level)).as_ptr() };

        loop {
            // Check if current node is null (end of list at this level)
            if curr.is_null() {
                return SearchResult { pred, curr };
            }

            unsafe {
                let next = (*curr).get_next(level);
                let next_marked = MarkedPtr::new(next);

                if next_marked.is_any_marked() {
                    // Current node is marked (DELETE or UPDATE)
                    // Try to physically remove it - for UPDATE, snip to replacement
                    // Use strong CAS to avoid spurious failures causing livelock
                    let snip = (*pred).cas_next(level, curr, next_marked.as_ptr());

                    if snip.is_err() {
                        // CAS failed - recover from level+1
                        pred = self.recover_pred(level, preds);

                        // Reload curr from recovered pred
                        curr = MarkedPtr::new((*pred).get_next(level)).as_ptr();

                        continue;
                    }

                    // Successfully removed, advance curr
                    curr = next_marked.as_ptr();
                    continue;
                }

                // INVARIANT: curr should never be the sentinel during traversal
                // If we encounter it, there's a bug in the algorithm
                debug_assert!(
                    !(*curr).is_sentinel(),
                    "INVARIANT VIOLATION: Encountered sentinel as curr during find_at_level!\n\
                     level={}, pred={:?}, curr={:?}, head={:?}",
                    level,
                    pred,
                    curr,
                    self.head
                );

                // NOTE: Unlike DELETE-marked nodes, UPDATE-marked nodes are NOT
                // specially handled here. Like sorted_list, we allow UPDATE-marked
                // nodes to become pred. CAS operations will fail when pred is
                // UPDATE-marked, and callers handle this via retry/backlinks.

                if (*curr).key() < key {
                    // Move forward
                    pred = curr;
                    curr = next_marked.as_ptr();
                } else {
                    // Found position (curr.key >= key)
                    return SearchResult { pred, curr };
                }
            }
        }
    }

    /// Link a new node at a specific level.
    ///
    /// Tries to CAS pred.next[level] from expected_succ to new_node.
    /// new_node.next[level] should already be set to expected_succ.
    ///
    /// Returns Ok(()) on success, Err(actual) on CAS failure.
    #[inline]
    unsafe fn link_at_level(
        &self,
        level: usize,
        pred: SkipNodePtr<T>,
        new_node: SkipNodePtr<T>,
        expected_succ: SkipNodePtr<T>,
    ) -> Result<(), *mut SkipNode<T>> {
        unsafe { (*pred).cas_next(level, expected_succ, new_node).map(|_| ()) }
    }

    /// Insert a new node at a specific level.
    ///
    /// Handles:
    /// - pred.next changed (concurrent insert - advance pred if smaller key)
    ///
    /// Returns true if successfully linked, false if:
    /// - new_node was marked (being deleted/updated)
    /// - pred.next is marked (being deleted - give up on this level)
    /// - CAS failed (concurrent modification - caller can give up)
    unsafe fn insert_at_level(
        &self,
        level: usize,
        pred: &mut SkipNodePtr<T>,
        new_node: SkipNodePtr<T>,
        node_key: &T,
        preds: &[SkipNodePtr<T>],
    ) -> bool {
        unsafe {
            loop {
                // Check if new_node is being deleted/updated at level 0
                let node_next_0 = (*new_node).get_next(0);
                if MarkedPtr::new(node_next_0).is_any_marked() {
                    return false; // Node is being removed, stop
                }

                // CRITICAL: Check if pred itself is being deleted (level 0 marked).
                if *pred != self.head {
                    let pred_next_0 = (**pred).get_next(0);
                    if MarkedPtr::new(pred_next_0).is_any_marked() {
                        *pred = self.recover_pred(level, preds);
                        continue;
                    }
                }

                let pred_next = (**pred).get_next(level);
                let pred_next_marked = MarkedPtr::new(pred_next);
                let pred_next_ptr = pred_next_marked.as_ptr();

                // If pred.next is marked at THIS level, recover from level+1
                if pred_next_marked.is_any_marked() {
                    *pred = self.recover_pred(level, preds);
                    continue;
                }

                // Advance pred if needed (concurrent insert of smaller key)
                // CRITICAL: Check height and level 0 - don't advance to a node being deleted
                if !pred_next_ptr.is_null()
                    && !(*pred_next_ptr).is_sentinel()
                    && (*pred_next_ptr).height() > level
                    && (*pred_next_ptr).key() < node_key
                {
                    // Check if this node is being deleted (level 0 marked)
                    let next_0 = (*pred_next_ptr).get_next(0);
                    if MarkedPtr::new(next_0).is_any_marked() {
                        // Node we'd advance to is being deleted - skip it
                        // Re-read pred.next, the deleted node will be unlinked
                        continue;
                    }
                    *pred = pred_next_ptr;
                    continue;
                }

                // If pred.next is already new_node, we're done
                if pred_next_ptr == new_node {
                    return true;
                }

                // Set new_node.next to current pred.next
                (*new_node).set_next(level, pred_next_ptr);

                // CAS pred.next from pred_next_ptr to new_node
                // On failure, retry - re-read pred.next and try again
                // NOTE: Don't set backlinks here - they're only set when marking for delete/update
                match (**pred).cas_next(level, pred_next_ptr, new_node) {
                    Ok(_) => return true,
                    Err(_) => continue, // CAS failed, retry the loop
                }
            }
        }
    }

    /// Unlink a node at a specific level with full retry logic.
    ///
    /// Handles:
    /// - pred.next != node (already unlinked or concurrent insert happened)
    /// - pred is marked (recover from level+1)
    /// - CAS failure with retry
    ///
    /// If `replacement` is provided, use it instead of `node.next[level]`.
    /// This is used for UPDATE at level 0 where replacement = new_node.
    ///
    /// Returns when node is unlinked at this level (or already was).
    unsafe fn unlink_at_level(
        &self,
        level: usize,
        pred: &mut SkipNodePtr<T>,
        node: SkipNodePtr<T>,
        node_key: &T,
        replacement: Option<SkipNodePtr<T>>,
        preds: &[SkipNodePtr<T>],
    ) {
        unsafe {
            loop {
                let pred_next = (**pred).get_next(level);
                let pred_next_marked = MarkedPtr::new(pred_next);
                let pred_next_ptr = pred_next_marked.as_ptr();

                if pred_next_ptr != node {
                    // pred.next != node: either unlinked or concurrent insert happened
                    // Defensive: check for sentinel (deallocated node)
                    if pred_next_ptr.is_null()
                        || (*pred_next_ptr).is_sentinel()
                        || (*pred_next_ptr).key() >= node_key
                    {
                        return; // Node is already unlinked at this level
                    }
                    // Concurrent insert happened, advance pred
                    // But at level 0, if the node we're advancing to is UPDATE-marked,
                    // we need to follow through to its replacement to avoid infinite loop
                    let mut new_pred = pred_next_ptr;
                    if level == 0 {
                        loop {
                            let new_pred_next = (*new_pred).get_next(0);
                            let new_pred_next_marked = MarkedPtr::new(new_pred_next);
                            if !new_pred_next_marked.is_update_marked() {
                                break;
                            }
                            // Follow through UPDATE-marked nodes
                            new_pred = new_pred_next_marked.as_ptr();
                        }
                    }
                    *pred = new_pred;
                    continue;
                }

                // pred.next == node, check if pred is marked (DELETE or UPDATE)
                if pred_next_marked.is_any_marked() {
                    // Recover from level+1
                    *pred = self.recover_pred(level, preds);
                    continue;
                }

                // Compute replacement: use provided or compute from node.next[level]
                let replacement_ptr = replacement.unwrap_or_else(|| {
                    let node_next = (*node).get_next(level);
                    MarkedPtr::unmask(node_next)
                });

                // Try to unlink: CAS pred.next from node to replacement
                match (**pred).cas_next(level, node, replacement_ptr) {
                    Ok(_) => return,
                    Err(actual) => {
                        let actual_ptr = MarkedPtr::unmask(actual);
                        if actual_ptr == node {
                            // pred is marked, recover from level+1
                            *pred = self.recover_pred(level, preds);
                            continue;
                        }
                        // Check if replacement is already linked
                        if actual_ptr == replacement_ptr {
                            return; // Already unlinked with this replacement
                        }
                        // Defensive: check for sentinel (deallocated node)
                        if actual_ptr.is_null()
                            || (*actual_ptr).is_sentinel()
                            || (*actual_ptr).key() >= node_key
                        {
                            return; // Already unlinked
                        }
                        *pred = actual_ptr;
                        continue;
                    }
                }
            }
        }
    }

    /// Find position for a key using proper top-down traversal.
    ///
    /// Returns (predecessors, successors) arrays for levels 0..max_height.
    /// This is O(log n) because we traverse from top level down,
    /// using the position found at each level as starting point for the next.
    ///
    /// # Position-based Optimization (O(1) amortized for sorted batch inserts)
    ///
    /// When `start_preds` is provided (predecessors at all levels from previous position):
    /// - For each level, start from start_preds[level] if valid (key < target and not marked)
    /// - For sorted batch inserts, each pred is typically ~1 node behind target
    /// - This gives O(1) amortized per insert because we only traverse forward ~1 node per level
    ///
    /// Fallback behavior:
    /// - If start_preds[level] is null or invalid, fall back to the pred from level+1
    /// - If no valid pred at any level, start from HEAD
    ///
    #[inline]
    fn find_position(
        &self,
        key: &T,
        max_height: usize,
        start_preds: Option<&[SkipNodePtr<T>; MAX_LEVEL]>,
    ) -> ([SkipNodePtr<T>; MAX_LEVEL], [SkipNodePtr<T>; MAX_LEVEL]) {
        let mut preds: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
        let mut succs: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];

        // Track the current predecessor as we descend through levels
        let mut last_pred = self.head;

        // Traverse from TOP level (self.max_level - 1) down to 0
        // This is critical for O(log n) - we must start from the top!
        for level in (0..self.max_level).rev() {
            // Try to use provided predecessor at this level for O(1) batch optimization
            if let Some(hint_preds) = start_preds {
                let hint_pred = hint_preds[level];
                if !hint_pred.is_null() {
                    unsafe {
                        // Check if hint pred is valid: not marked and key < target
                        let hint_next = (*hint_pred).get_next(level);
                        let is_marked = MarkedPtr::new(hint_next).is_marked();
                        // Sentinel has no key, treat it as valid (less than anything)
                        let key_ok = (*hint_pred).is_sentinel() || (*hint_pred).key() < key;
                        if !is_marked && key_ok && (*hint_pred).height() > level {
                            last_pred = hint_pred;
                        }
                    }
                }
            }

            // last_pred always has links at current level because:
            // - Initial: head has MAX_LEVEL height
            // - After find_at_level: returned pred has height > that level
            //   so it also has height > (level - 1) for next iteration
            // - Hint pred is only used if height > level
            unsafe {
                if (*last_pred).height() <= level {
                    panic!(
                        "INVARIANT VIOLATION: last_pred height {} <= level {}\n\
                         last_pred={:?}, is_sentinel={}",
                        (*last_pred).height(),
                        level,
                        last_pred,
                        (*last_pred).is_sentinel()
                    );
                }
            }

            let result = self.find_at_level(key, level, Some(last_pred), &preds);

            // Update last_pred for next level
            last_pred = result.pred;

            // Only record preds/succs for levels we need (0..max_height)
            if level < max_height {
                preds[level] = result.pred;
                succs[level] = result.curr;
            }
        }

        (preds, succs)
    }

    /// Internal insert implementation
    ///
    /// Takes ownership of key. On CAS failure, takes value back for retry.
    ///
    /// # Position-based Optimization (O(1) amortized for sorted batch inserts)
    ///
    /// When `start_preds` is provided (predecessors at all levels from previous position),
    /// the search starts from these predecessors, giving O(1) amortized insertion
    /// for sorted batch inserts.
    fn insert_internal(
        &self,
        key: T,
        start_preds: Option<&[SkipNodePtr<T>; MAX_LEVEL]>,
    ) -> (Option<*mut SkipNode<T>>, [SkipNodePtr<T>; MAX_LEVEL]) {
        // Delegate to insert_internal_with_height with random height
        self.insert_internal_with_height(key, Self::random_level(), start_preds)
    }

    /// Internal insert implementation with specified height.
    /// Used for sentinel nodes that need MAX_LEVEL height.
    /// Also used by insert_internal with random height.
    ///
    fn insert_internal_with_height(
        &self,
        mut key: T,
        height: usize,
        start_preds: Option<&[SkipNodePtr<T>; MAX_LEVEL]>,
    ) -> (Option<*mut SkipNode<T>>, [SkipNodePtr<T>; MAX_LEVEL]) {
        'retry: loop {
            // Find position at all levels, using start preds if available
            let (mut preds, succs) = self.find_position(&key, height, start_preds);

            // Check for duplicates at level 0
            if !succs[0].is_null() {
                unsafe {
                    if (*succs[0]).is_sentinel() {
                        continue 'retry;
                    }
                    if (*succs[0]).key() == &key {
                        return (None, preds);
                    }
                }
            }

            // Create new node with specified height
            let new_node = SkipNode::alloc_with_key(key, height);

            unsafe {
                for (level, &succ) in succs.iter().enumerate().take(height) {
                    (*new_node).set_next(level, succ);
                }

                let result = (*preds[0]).cas_next(0, succs[0], new_node);

                if result.is_err() {
                    // Safe: node was never linked, we have exclusive ownership
                    key = (*new_node).take_value_unlinked();
                    SkipNode::dealloc_node(new_node);
                    continue 'retry;
                }

                let node_key = (*new_node).key();

                for level in 1..height {
                    let mut pred = preds[level];
                    if !self.insert_at_level(level, &mut pred, new_node, node_key, &preds) {
                        break;
                    }
                    preds[level] = pred;
                }

                return (Some(new_node), preds);
            }
        }
    }

    /// Internal remove implementation.
    ///
    fn remove_internal(
        &self,
        key: &T,
        start_preds: Option<&[SkipNodePtr<T>; MAX_LEVEL]>,
    ) -> Option<*mut SkipNode<T>> {
        // Find the node using O(log n) traversal, with optional start preds
        let (preds, succs) = self.find_position(key, self.max_level, start_preds);

        if succs[0].is_null() {
            return None;
        }

        unsafe {
            // Defensive check: skip deallocated nodes
            if (*succs[0]).is_sentinel() {
                return None;
            }
            if (*succs[0]).key() != key {
                return None;
            }

            let node = succs[0];
            let height = (*node).height();

            // Check if node is fully linked by verifying the highest level.
            // Insert links bottom-up, so if highest level is linked, all lower levels are too.
            if height > 1 && succs[height - 1] != node {
                // Node not fully linked yet - still being inserted.
                return None;
            }

            // =================================================================
            // Level-by-level DELETE: For each level (top-down):
            //   1. Mark (CAS to add DEL)
            //   2. Unlink
            // Level 0 mark determines ownership.
            // Backlinks disabled - recovery restarts from HEAD.
            // =================================================================

            let node_key = (*node).key();

            // Process higher levels first (height-1 down to 1)
            // Any thread can help here - no ownership yet
            for level in (1..height).rev() {
                // 1. Mark this level (CAS to add DEL flag)
                loop {
                    let next = (*node).get_next(level);
                    let next_marked = MarkedPtr::new(next);

                    if next_marked.is_any_marked() {
                        // Already marked by another thread - skip
                        break;
                    }

                    let marked = next_marked.with_mark(true);
                    if (*node).cas_next_weak(level, next, marked.as_raw()).is_ok() {
                        break;
                    }
                    // CAS failed, retry
                }

                // 2. Unlink at this level
                let mut pred = preds[level];
                self.unlink_at_level(level, &mut pred, node, node_key, None, &preds);
            }

            // Process level 0 - this determines OWNERSHIP
            // 1. Mark level 0 (ownership CAS)
            // Check is_any_marked() to handle both DELETE and UPDATE marks:
            // - DELETE_MARK: another thread is deleting this node
            // - UPDATE_MARK: another thread is updating this node (forward insertion)
            // First thread to mark level 0 wins ownership.
            let we_own = loop {
                let next = (*node).get_next(0);
                let next_marked = MarkedPtr::new(next);

                if next_marked.is_any_marked() {
                    // Already marked by another thread (DELETE or UPDATE) - they own it
                    break false;
                }

                let marked = next_marked.with_mark(true);
                if (*node).cas_next_weak(0, next, marked.as_raw()).is_ok() {
                    break true;
                }
                // CAS failed, retry
            };

            if !we_own {
                return None;
            }

            // 2. Unlink at level 0 - we own it
            let mut pred = preds[0];
            self.unlink_at_level(0, &mut pred, node, node_key, None, &preds);

            // Node fully unlinked - safe to return
            Some(node)
        }
    }
}

impl<T: Ord> Default for SkipList<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for SkipList<T> {
    fn drop(&mut self) {
        unsafe {
            let mut curr = (*self.head).get_next(0);
            curr = MarkedPtr::unmask(curr);

            while !curr.is_null() {
                let next = (*curr).get_next(0);
                let next_marked = MarkedPtr::new(next);

                // INVARIANT CHECK: No DELETE-marked nodes should remain at drop time
                // (UPDATE-marked nodes that are also DELETE-marked should have been unlinked)
                // Note: We now allow UPDATE-only marked nodes since the new node is linked
                if next_marked.is_marked() && !(*curr).is_sentinel() {
                    panic!(
                        "INVARIANT VIOLATION: Found DELETE-marked node at drop time!\n\
                         Marked nodes should have been physically unlinked before drop."
                    );
                }

                let next_clean = next_marked.as_ptr();
                SkipNode::dealloc_node(curr);
                curr = next_clean;
            }

            SkipNode::dealloc_node(self.head);
        }
    }
}

// Safety: SkipList is thread-safe
unsafe impl<T: Send> Send for SkipList<T> {}
unsafe impl<T: Send> Sync for SkipList<T> {}

// ============================================================================
// SortedCollection trait implementation
// ============================================================================

impl<T: Ord> SortedCollection<T> for SkipList<T> {
    type Node = SkipNode<T>;
    type NodePosition = SkipNodePosition<T>;

    fn insert_from_internal(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition> {
        // Extract preds from position if available
        let start_preds = position.map(|pos| pos.preds());
        let (node_opt, preds) = self.insert_internal(key, start_preds);

        node_opt.map(|node| SkipNodePosition::new(preds, node))
    }

    fn remove_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
    ) -> Option<Self::NodePosition> {
        // Extract preds from position if available
        let start_preds = position.map(|pos| pos.preds());
        self.remove_internal(key, start_preds)
            .map(|node| SkipNodePosition::from_node(node))
    }

    fn find_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
        is_match: bool,
    ) -> Option<Self::NodePosition> {
        // Extract preds from position if available
        let start_preds = position.map(|pos| pos.preds());
        let (preds, succs) = self.find_position(key, MAX_LEVEL, start_preds);

        if is_match {
            // Exact match required
            if !succs[0].is_null() {
                unsafe {
                    // Defensive check: skip deallocated nodes
                    if (*succs[0]).is_sentinel() {
                        return None;
                    }
                    if (*succs[0]).key() == key {
                        return Some(SkipNodePosition::new(preds, succs[0]));
                    }
                }
            }
            None
        } else {
            // Return position at insertion point (preds)
            Some(SkipNodePosition::new(preds, preds[0]))
        }
    }

    /// Update an existing key's value atomically.
    ///
    /// # Algorithm (Mark-then-Insert UPDATE)
    ///
    /// ```text
    /// Before: preds[0] → curr → succ
    /// Goal:   preds[0] → new_node → succ (curr orphaned)
    ///
    /// Step 1: Unlink higher levels (mark + unlink curr at levels 1..height)
    ///         After: curr only linked at level 0
    ///
    /// Step 2: Mark curr.next[0] with UPDATE_MARK (ownership)
    ///         curr.next[0] still points to succ (just marked)
    ///         After: preds[0] → curr(UPDATE) → succ
    ///
    /// Step 3: Insert new_node (sees curr as marked/deleted, inserts at same position)
    ///         After: preds[0] → new_node → succ
    ///                curr is orphaned (insert unlinked it)
    ///
    /// Step 4: Unlink curr at level 0 (may already be done by insert)
    /// ```
    ///
    /// # Memory Safety
    ///
    /// Unlike forward-insertion, curr.next[0] points to SUCC (not new_node).
    /// If curr gets freed, readers following curr.next[0] still get a valid node.
    /// The new value is found through normal traversal, not by following curr.
    ///
    fn update_internal(
        &self,
        position: Option<&Self::NodePosition>,
        new_value: T,
    ) -> Option<(*mut Self::Node, Self::NodePosition)> {
        let start_preds = position.map(|pos| pos.preds());

        // Find the node to update
        let (preds, succs) = self.find_position(&new_value, self.max_level, start_preds);

        if succs[0].is_null() {
            return None;
        }

        unsafe {
            // Defensive check
            if (*succs[0]).is_sentinel() || (*succs[0]).key() != &new_value {
                return None;
            }

            let curr = succs[0];
            let height = (*curr).height();
            let curr_key = (*curr).key();

            // Check if node is fully linked
            if height > 1 && succs[height - 1] != curr {
                return None; // Node still being inserted
            }

            // ================================================================
            // MARK-THEN-INSERT UPDATE PROTOCOL
            // ================================================================

            // Step 1: Unlink higher levels first (top-down)
            // This reduces contention - curr becomes only visible at level 0
            for level in (1..height).rev() {
                // Mark this level
                loop {
                    let next = (*curr).get_next(level);
                    let next_marked = MarkedPtr::new(next);

                    if next_marked.is_any_marked() {
                        break; // Already marked
                    }

                    let marked = next_marked.with_mark(true);
                    if (*curr).cas_next_weak(level, next, marked.as_raw()).is_ok() {
                        break;
                    }
                }

                // Unlink at this level
                let mut pred = preds[level];
                self.unlink_at_level(level, &mut pred, curr, curr_key, None, &preds);
            }

            // Step 2: Mark level 0 with UPDATE_MARK (ownership)
            // Note: curr.next[0] still points to succ, just marked
            let we_own = loop {
                let next = (*curr).get_next(0);
                let next_marked = MarkedPtr::new(next);

                if next_marked.is_any_marked() {
                    // Already marked by DELETE or UPDATE - they own it
                    break false;
                }

                // Mark with UPDATE_MARK, pointer still points to succ
                let marked = next_marked.with_update_mark(true);
                if (*curr).cas_next_weak(0, next, marked.as_raw()).is_ok() {
                    break true;
                }
            };

            if !we_own {
                return None;
            }

            // Step 3: Insert new_node
            // The insert will see curr as UPDATE-marked and skip/unlink it
            // It will insert new_node at the position where curr was
            let (new_node_opt, new_preds) = self.insert_internal(new_value, Some(&preds));

            match new_node_opt {
                Some(new_node) => {
                    // Insert succeeded - curr was unlinked by insert
                    Some((curr, SkipNodePosition::new(new_preds, new_node)))
                }
                None => {
                    // Insert failed (shouldn't happen since we marked curr)
                    // But if it does, we still own curr, need to clean up
                    let mut pred = preds[0];
                    self.unlink_at_level(0, &mut pred, curr, curr_key, None, &preds);
                    None
                }
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

        if curr.is_null() || curr == self.head {
            return None;
        }

        unsafe { Some(f((*curr).key())) }
    }

    fn first_node_internal(&self) -> Option<*mut Self::Node> {
        unsafe {
            // Start from head's next at level 0 (bottom level has all nodes)
            let mut curr = (*self.head).get_next(0);
            curr = MarkedPtr::unmask(curr);

            // Skip marked nodes, follow UPDATE-marked next pointers
            while !curr.is_null() {
                let next = (*curr).get_next(0);
                let next_marked = MarkedPtr::new(next);

                if next_marked.is_update_marked() && !next_marked.is_marked() {
                    // UPDATE-marked only - follow next[0] to new node (forward insertion)
                    let new_node = next_marked.as_ptr();
                    if !new_node.is_null() && new_node != self.head {
                        return Some(new_node);
                    }
                }

                if !next_marked.is_any_marked() {
                    return Some(curr);
                }
                curr = next_marked.as_ptr();
            }

            None
        }
    }

    fn next_node_internal(&self, node: *mut Self::Node) -> Option<*mut Self::Node> {
        let node = MarkedPtr::unmask(node);

        if node.is_null() {
            return None;
        }

        unsafe {
            // Get next pointer at level 0, skip marked nodes
            let mut curr = (*node).get_next(0);
            curr = MarkedPtr::unmask(curr);

            while !curr.is_null() {
                // Check if this node is marked
                let next = (*curr).get_next(0);
                let next_marked = MarkedPtr::new(next);

                if next_marked.is_update_marked() && !next_marked.is_marked() {
                    // UPDATE-marked only - follow next[0] to new node (forward insertion)
                    let new_node = next_marked.as_ptr();
                    if !new_node.is_null() && new_node != self.head {
                        return Some(new_node);
                    }
                }

                if !next_marked.is_any_marked() {
                    return Some(curr);
                }

                // Node is marked, continue to next
                curr = next_marked.as_ptr();
            }
        }

        None
    }

    /// Insert a sentinel node with MAX_LEVEL height.
    ///
    /// For skip lists, sentinels need MAX_LEVEL height to enable O(log n)
    /// searches within buckets in split-ordered hash tables.
    ///
    fn insert_sentinel(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition> {
        let start_preds = position.map(|pos| pos.preds());
        let (node_opt, preds) = self.insert_internal_with_height(key, MAX_LEVEL, start_preds);

        node_opt.map(|node| SkipNodePosition::new(preds, node))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::{SkipList, SkipNode};
    use crate::data_structures::{CollectionNode, NodePosition, SortedCollection};

    // Common tests (test_basic_operations, test_concurrent_operations, etc.)
    // are in sorted_collection_core_tests.rs and run via deferred_collection_tests.rs

    #[test]
    fn test_trait_methods() {
        // Tests SortedCollection trait methods directly on SkipList
        let list = SkipList::new();

        // insert_from_internal
        assert!(list.insert_from_internal(5, None).is_some());
        assert!(list.insert_from_internal(3, None).is_some());
        assert!(list.insert_from_internal(5, None).is_none()); // Duplicate

        // find_from_internal
        assert!(list.find_from_internal(None, &5, true).is_some());
        assert!(list.find_from_internal(None, &99, true).is_none());

        // remove_from_internal - get pointer for cleanup
        let deleted_node = list.remove_from_internal(None, &5);
        assert!(deleted_node.is_some());
        assert!(list.find_from_internal(None, &5, true).is_none());

        // Clean up deleted node to avoid memory leak
        unsafe {
            if let Some(pos) = deleted_node {
                SkipNode::dealloc_node(pos.node_ptr());
            }
        }
    }

    #[test]
    fn test_basic_insert_delete() {
        // Basic test: insert values and delete some
        let list = SkipList::new();

        // Insert some nodes
        for i in 0..20 {
            list.insert(i);
        }

        // Verify all values exist
        for i in 0..20 {
            assert!(list.contains(&i), "Value {} should exist", i);
        }

        // Delete some values
        for i in (0..20).step_by(2) {
            let deleted = list.remove_from_internal(None, &i);
            assert!(deleted.is_some(), "Should delete {}", i);
            // Clean up deleted node
            unsafe {
                if let Some(pos) = deleted {
                    SkipNode::dealloc_node(pos.node_ptr());
                }
            }
        }

        // Verify remaining values
        for i in 0..20 {
            if i % 2 == 0 {
                assert!(!list.contains(&i), "Even {} should be deleted", i);
            } else {
                assert!(list.contains(&i), "Odd {} should still exist", i);
            }
        }

        println!("Basic insert/delete test passed");
    }

    #[test]
    fn test_marked_node_recovery() {
        // Tests recovery from a marked starting node using preds[level+1]
        let list = SkipList::new();

        // Insert nodes
        for i in 0..100 {
            list.insert(i);
        }

        // Get pointer to node 50
        let node_50 = list.find_from_internal(None, &50, true).unwrap();

        // Delete node 50 - get pointer for cleanup
        let deleted_node = list.remove_from_internal(None, &50);
        assert!(deleted_node.is_some());

        // Try to search starting from marked node 50
        // Should recover from level+1 (or HEAD if no preds)
        let empty_preds: [*mut SkipNode<i32>; 16] = [std::ptr::null_mut(); 16];
        let result = list.find_at_level(&60, 0, Some(node_50.node_ptr()), &empty_preds);

        assert!(!result.curr.is_null());
        unsafe {
            assert_eq!(*(*result.curr).key(), 60);

            // Clean up deleted node to avoid memory leak
            if let Some(pos) = deleted_node {
                SkipNode::dealloc_node(pos.node_ptr());
            }
        }

        println!("Successfully recovered from marked starting node");
    }

    // =========================================================================
    // Update operation tests
    // =========================================================================
    // TODO: Update tests are disabled until update_internal is implemented for SkipListBacklinks

    /*
    #[test]
    fn test_update_basic() {
        // Basic test: insert, update, verify new value
        let list = SkipList::new();

        // Insert initial values
        list.insert(10);
        list.insert(20);
        list.insert(30);

        // Verify initial state
        assert!(list.contains(&10));
        assert!(list.contains(&20));
        assert!(list.contains(&30));

        // Update value 20 (note: in this simple case, key=value)
        // Since key equals value, updating 20 with 20 should work
        let result = list.update_internal(None, &20, 20);
        assert!(result.is_some(), "Update should succeed");

        // Verify the key still exists
        assert!(list.contains(&20));

        // Other values unaffected
        assert!(list.contains(&10));
        assert!(list.contains(&30));

        println!("Basic update test passed");
    }

    #[test]
    fn test_update_nonexistent() {
        // Test updating a key that doesn't exist
        let list = SkipList::new();

        list.insert(10);
        list.insert(30);

        // Try to update key 20 which doesn't exist
        let result = list.update_internal(None, &20, 20);
        assert!(result.is_none(), "Update of non-existent key should return None");

        // Original values still exist
        assert!(list.contains(&10));
        assert!(list.contains(&30));
        assert!(!list.contains(&20));

        println!("Update non-existent key test passed");
    }

    #[test]
    fn test_update_sequential() {
        // Test multiple sequential updates
        let list = SkipList::new();

        // Insert values
        for i in 0..100 {
            list.insert(i);
        }

        // Update every 10th element
        for i in (0..100).step_by(10) {
            let result = list.update_internal(None, &i, i);
            assert!(result.is_some(), "Update of {} should succeed", i);
        }

        // All values should still exist
        for i in 0..100 {
            assert!(list.contains(&i), "Value {} should exist", i);
        }

        println!("Sequential update test passed");
    }

    #[test]
    fn test_update_preserves_order() {
        // Test that updates don't disrupt list order
        let list = SkipList::new();

        // Insert values
        for i in 0..50 {
            list.insert(i);
        }

        // Update middle values
        for i in 20..30 {
            list.update_internal(None, &i, i);
        }

        // Verify order by checking first_node_internal and next_node_internal
        let mut expected = 0;
        let mut node = list.first_node_internal();
        while let Some(n) = node {
            unsafe {
                assert_eq!(*(*n).key(), expected, "Order disrupted at {}", expected);
            }
            expected += 1;
            node = list.next_node_internal(n);
        }
        assert_eq!(expected, 50, "Should have traversed all 50 nodes");

        println!("Update preserves order test passed");
    }

    #[test]
    fn test_update_first_node() {
        // Test updating the first node after sentinel (edge case)
        let list = SkipList::new();

        // Insert values - 1 will be first after sentinel
        list.insert(1);
        list.insert(5);
        list.insert(10);

        // Verify 1 is first
        let first = list.first_node_internal();
        assert!(first.is_some());
        unsafe {
            assert_eq!(*(*first.unwrap()).key(), 1);
        }

        // Update the first node
        let result = list.update_internal(None, &1, 1);
        assert!(result.is_some(), "Update of first node should succeed");

        // Verify 1 is still first and accessible
        assert!(list.contains(&1));
        let first_after = list.first_node_internal();
        assert!(first_after.is_some());
        unsafe {
            assert_eq!(*(*first_after.unwrap()).key(), 1);
        }

        // Verify traversal still works
        let mut count = 0;
        let mut node = list.first_node_internal();
        while let Some(n) = node {
            count += 1;
            node = list.next_node_internal(n);
        }
        assert_eq!(count, 3, "Should have 3 nodes after update");

        println!("Update first node test passed");
    }

    #[test]
    fn test_update_concurrent() {
        // Test concurrent updates from multiple threads
        use std::sync::Arc;
        use std::thread;

        let list = Arc::new(SkipList::new());

        // Insert initial values
        for i in 0..1000 {
            list.insert(i);
        }

        let num_threads = 4;
        let updates_per_thread = 100;
        let mut handles = vec![];

        for t in 0..num_threads {
            let list_clone = Arc::clone(&list);
            let handle = thread::spawn(move || {
                for i in 0..updates_per_thread {
                    let key = (t * updates_per_thread + i) % 1000;
                    let _ = list_clone.update_internal(None, &key, key);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify all original keys still exist
        for i in 0..1000 {
            assert!(list.contains(&i), "Key {} should still exist", i);
        }

        println!("Concurrent update test passed");
    }

    #[test]
    fn test_update_with_deletes() {
        // Test updates interleaved with deletes
        let list = SkipList::new();

        // Insert values
        for i in 0..100 {
            list.insert(i);
        }

        // Update even numbers, delete odd numbers
        for i in 0..100 {
            if i % 2 == 0 {
                list.update_internal(None, &i, i);
            } else {
                let deleted = list.remove_from_internal(None, &i);
                if let Some(pos) = deleted {
                    unsafe {
                        SkipNode::dealloc_node(pos.node_ptr());
                    }
                }
            }
        }

        // Verify: evens exist, odds don't
        for i in 0..100 {
            if i % 2 == 0 {
                assert!(list.contains(&i), "Even {} should exist", i);
            } else {
                assert!(!list.contains(&i), "Odd {} should not exist", i);
            }
        }

        println!("Update with deletes test passed");
    }
    */

    // ========================================================================
    // INSERT/DELETE Cooperation Tests
    // ========================================================================
    // These tests verify that INSERT correctly stops adding higher levels
    // when it detects the node is being deleted, and that DELETE handles
    // partially-linked nodes correctly.

    #[test]
    fn test_insert_delete_cooperation_basic() {
        // Test that concurrent insert and delete on the same key works correctly
        use std::sync::Arc;
        use std::thread;

        let list = Arc::new(SkipList::new());

        // Pre-populate with some values
        for i in 0..100 {
            list.insert(i);
        }

        let list1 = Arc::clone(&list);
        let list2 = Arc::clone(&list);

        // Thread 1: Insert new values
        let inserter = thread::spawn(move || {
            for i in 100..200 {
                list1.insert(i);
            }
        });

        // Thread 2: Delete some of the values being inserted
        let deleter = thread::spawn(move || {
            for i in 100..200 {
                // Try to delete - might succeed if insert happened first
                if let Some(pos) = list2.remove_from_internal(None, &i) {
                    unsafe {
                        SkipNode::dealloc_node(pos.node_ptr());
                    }
                }
            }
        });

        inserter.join().unwrap();
        deleter.join().unwrap();

        // Verify list is consistent - all remaining values should be findable
        let mut count = 0;
        let mut node = list.first_node_internal();
        while let Some(n) = node {
            count += 1;
            node = list.next_node_internal(n);
        }

        // At least the original 100 values should exist
        assert!(
            count >= 100,
            "Should have at least 100 values, found {}",
            count
        );

        println!(
            "Insert/delete cooperation basic test passed: {} nodes",
            count
        );
    }

    #[test]
    fn test_insert_delete_cooperation_high_contention() {
        // High contention: many threads inserting and deleting same keys
        // Uses DeferredCollection for safe memory reclamation
        use crate::data_structures::SafeSortedCollection;
        use crate::data_structures::wrappers::DeferredCollection;
        use std::sync::Arc;
        use std::thread;

        let list: Arc<DeferredCollection<i32, SkipList<i32>>> =
            Arc::new(DeferredCollection::default());
        let num_threads: i32 = 8;
        let ops_per_thread: i32 = 100;

        let mut handles = vec![];

        for t in 0..num_threads {
            let list_clone = Arc::clone(&list);
            let handle = thread::spawn(move || {
                for i in 0..ops_per_thread {
                    let key = (t * 1000 + i) % 50; // Limited key space for contention

                    if i % 2 == 0 {
                        list_clone.insert(key);
                    } else {
                        list_clone.delete(&key);
                    }
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify list is consistent (sorted order)
        let values: Vec<i32> = list.iter().map(|r| *r).collect();
        for window in values.windows(2) {
            assert!(
                window[0] < window[1],
                "List not sorted: {} followed by {}",
                window[0],
                window[1]
            );
        }

        println!("Insert/delete cooperation high contention test passed");
    }

    #[test]
    fn test_insert_delete_rapid_cycle() {
        // Rapidly insert and delete the same key
        // Uses DeferredCollection to handle safe deallocation of deleted nodes
        use crate::data_structures::SafeSortedCollection;
        use crate::data_structures::wrappers::DeferredCollection;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        // DeferredCollection wraps SkipList and defers node destruction until drop
        let list = Arc::new(DeferredCollection::new(SkipList::new()));
        let successful_inserts = Arc::new(AtomicUsize::new(0));
        let successful_deletes = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];

        // Multiple threads all operating on key 42
        for _ in 0..4 {
            let list_clone = Arc::clone(&list);
            let inserts = Arc::clone(&successful_inserts);
            let deletes = Arc::clone(&successful_deletes);

            let handle = thread::spawn(move || {
                for _ in 0..1000 {
                    // Try to insert
                    if list_clone.insert(42) {
                        inserts.fetch_add(1, Ordering::Relaxed);
                    }

                    // Try to delete - DeferredCollection handles safe deallocation
                    if list_clone.delete(&42) {
                        deletes.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let ins = successful_inserts.load(Ordering::Relaxed);
        let del = successful_deletes.load(Ordering::Relaxed);

        // The key should either exist or not exist
        let exists = list.contains(&42);

        // Invariant: inserts - deletes should be 0 or 1
        let diff = ins as i64 - del as i64;
        assert!(
            diff == 0 || diff == 1,
            "Invariant violated: inserts={}, deletes={}, diff={}, exists={}",
            ins,
            del,
            diff,
            exists
        );

        // If diff is 1, key should exist; if diff is 0, key should not exist
        if diff == 1 {
            assert!(exists, "Key should exist when inserts - deletes = 1");
        } else {
            assert!(!exists, "Key should not exist when inserts - deletes = 0");
        }

        println!(
            "Insert/delete rapid cycle test passed: inserts={}, deletes={}, exists={}",
            ins, del, exists
        );

        // DeferredCollection will deallocate all deleted nodes when dropped
    }

    #[test]
    fn test_partially_linked_node_cleanup() {
        // Test that delete handles nodes that might not be fully linked at all levels
        // Uses DeferredCollection to handle safe deallocation of deleted nodes
        use crate::data_structures::SafeSortedCollection;
        use crate::data_structures::wrappers::DeferredCollection;
        use std::sync::Arc;
        use std::sync::Barrier;
        use std::thread;

        // DeferredCollection wraps SkipList and defers node destruction until drop
        let list = Arc::new(DeferredCollection::new(SkipList::new()));
        let barrier = Arc::new(Barrier::new(3));

        // Pre-populate
        for i in 0..50 {
            list.insert(i * 2); // Even numbers
        }

        let list1 = Arc::clone(&list);
        let list2 = Arc::clone(&list);
        let list3 = Arc::clone(&list);
        let barrier1 = Arc::clone(&barrier);
        let barrier2 = Arc::clone(&barrier);
        let barrier3 = Arc::clone(&barrier);

        // Thread 1: Insert odd numbers
        let inserter = thread::spawn(move || {
            barrier1.wait();
            for i in 0..50 {
                list1.insert(i * 2 + 1);
            }
        });

        // Thread 2: Delete all numbers - DeferredCollection handles safe deallocation
        let deleter1 = thread::spawn(move || {
            barrier2.wait();
            for i in 0..100 {
                list2.delete(&i);
            }
        });

        // Thread 3: Also delete - DeferredCollection handles safe deallocation
        let deleter2 = thread::spawn(move || {
            barrier3.wait();
            for i in (0..100).rev() {
                list3.delete(&i);
            }
        });

        inserter.join().unwrap();
        deleter1.join().unwrap();
        deleter2.join().unwrap();

        // Verify list is still consistent (might be empty or have some values)
        let mut count = 0;
        let mut prev_key: Option<i32> = None;
        let mut node = list.inner().first_node_internal();
        while let Some(n) = node {
            count += 1;
            unsafe {
                let key = *(*n).key();
                if let Some(prev) = prev_key {
                    assert!(key > prev, "List not sorted after partial link cleanup");
                }
                prev_key = Some(key);
            }
            node = list.inner().next_node_internal(n);
        }

        println!(
            "Partially linked node cleanup test passed: {} nodes remaining",
            count
        );

        // DeferredCollection will deallocate all deleted nodes when dropped
    }
}
