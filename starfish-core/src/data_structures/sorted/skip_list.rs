//! Lock-free skip list with Harris-style logical deletion.
//!
//! Implements a concurrent sorted collection using a probabilistic skip list
//! (32 levels, p=0.5). Supports insert, delete, update, and find with
//! linearizable semantics; all presence decisions share ONE resolution
//! (`SkipNode::is_visible` / `resolve_replacement` — see the protocol block
//! below). Parameterized by `Guard` for memory reclamation.
//!
//! Map mode — TWO backends, both epoch-safe:
//! - `SkipList<MapEntry<K, V>>`: value-CAS `update` (single CAS on the entry's
//!   value pointer) and claim-first `remove` (value tombstone = linearization
//!   point) — update↔remove races serialize on the value pointer, never losing
//!   an update. `get_ref` provides zero-copy guarded value reads. No
//!   `UPDATE_MARK` on these lists.
//! - `SkipList<Pair<K, V>>`: node-replacement `update` (forward splice via
//!   `update_internal` — one CAS marks the old carrier `UPDATE_MARK` and
//!   links its same-key replacement, so the key is never absent) with the value
//!   INLINE in the node; update↔remove serialize on the carrier's level-0 next
//!   pointer instead.
//!
//! Key invariants: sorted at every level (non-strict at routing levels while a
//! deleted "zombie" awaits its physical phase); keys unique among unmarked nodes;
//! marked nodes are physically unlinked before retirement; a node's tower is
//! written only by its inserter, so the physical delete phase (full-tower mark +
//! unlink + retire) runs only under tower quiescence, arbitrated by the
//! `linked_height` RMW handshake. No operation ever waits on another thread's
//! progress: insert fails fast on an unmarked duplicate (even one still linking
//! its tower), delete returns at its level-0 mark, and all cleanup is helped.

use std::alloc::{Layout, alloc, dealloc};
use std::borrow::Borrow;
use std::mem::MaybeUninit;
use std::ptr;

use crate::data_structures::hash::map_collection::{MapCollection, MapCollectionInternal, MapNode};
use crate::data_structures::map_entry::MapEntry;
use crate::data_structures::pair::Pair;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};

use super::load_consume;
use super::prefetch_read;

use crate::data_structures::{
    CollectionNode, MarkedPtr, NodePosition, SortedCollection, SortedCollectionInternal,
};
use crate::guard::Guard;

const MAX_LEVEL: usize = 32;

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
// VISIBILITY & LINEARIZATION (one predicate, no waiting)
// =============================================================================
//
// A node is a VISIBLE member iff its level-0 next pointer is unmarked (see
// `SkipNode::is_visible`). The record exists from the moment the level-0
// publishing CAS lands; the upper tower is a routing accelerator, never part of
// presence. Every presence decision uses this one predicate: contains/find
// (find_node_internal), exact-match find_from_internal, the iteration helpers,
// insert's duplicate decision, and delete's eligibility. Ad-hoc per-site gates
// produced non-linearizable histories (insert says "exists" while get says
// "absent" — non-linearizable).
//
// Linearization points:
//   - insert:  its successful level-0 publishing CAS. A physically-present
//     UNMARKED duplicate — even one whose tower is still being linked — makes
//     insert fail IMMEDIATELY ("exists"); reads agree it is present. A MARKED
//     duplicate is helped out of the way (the re-traversal snips it) and the
//     insert retries. Insert never waits on another thread's progress.
//   - delete:  the level-0 DELETE mark (ownership CAS). It needs no
//     reachability or publication precondition (the old reachability gate is gone
//     entirely): level-0 slots are only ever CAS'd after the publishing CAS, so
//     the mark cannot be erased, and delete returns at the mark. The PHYSICAL
//     phase (upper-level marks, unlink, retirement) is decoupled — see below.
//   - reads:   evaluation of the predicate on the candidate node.
//
// LOGICAL vs PHYSICAL DELETE — the linked_height handshake
// ─────────────────────────────────────────────────────────────────────────────
// Upper tower slots of an unlinked level are written with PLAIN stores by their
// single writer, the inserter (`set_next_relaxed`). Marking/unlinking upper
// levels while those stores may still be in flight could erase a DELETE mark or
// re-link an already-retired node (use-after-free under real reclamation). The
// physical phase therefore runs only under TOWER QUIESCENCE, decided by two
// RMWs on the SAME atomic (`linked_height`, AcqRel — its single modification
// order is the arbiter; no fences needed):
//
//   inserter (after its linking loop, even if aborted): swap(height)
//   deleter  (after winning the level-0 mark):          fetch_or(DEL_PENDING)
//
//   swap returned DEL_PENDING set  → a delete landed mid-linking: the INSERTER
//                                    runs the physical phase (it helps complete
//                                    the delete — it is the one thread that
//                                    knows its own tower is now quiescent).
//   fetch_or returned height       → tower already quiescent: the DELETER runs
//                                    the physical phase itself.
//
// Exactly one of the two observes the other's RMW first, so the physical phase
// — and the node's retirement (`physical_delete`) — runs exactly once. Neither
// side ever waits: delete returns at its mark; insert returns after its swap
// (plus the teardown it may have inherited). NO OPERATION ON THIS LIST EVER
// SPINS ON ANOTHER THREAD'S PROGRESS.
//
// A level-0-marked node whose physical phase is still pending is a "zombie":
// invisible to every read, snipped at level 0 by any traversal that meets it,
// and possibly still linked (unmarked) at upper levels where traversals simply
// route through it. A same-key insert can therefore briefly coexist with a
// zombie at upper levels; level lists stay sorted (non-strict at routing
// levels) and all presence answers come from level 0, so this is benign.
//
// =============================================================================
// UPDATE — two backends: value-CAS (MapEntry) and node-replacement (Pair)
// =============================================================================
//
// `SkipList<MapEntry<K, V>>`: the value lives behind an `AtomicPtr<V>` in the
// node's payload, `update` is a single value-pointer CAS (retiring the old
// value through the guard), and `remove` CLAIMS the value (null tombstone) as
// its linearization point before physically deleting the node. The node is
// never replaced, so updates have no tower/marking interaction at all, and
// `UPDATE_MARK` never appears on a MapEntry list.
//
// `SkipList<Pair<K, V>>`: node-replacement UPDATE via `update_internal` (the
// forward-splice protocol — see its section comment below): one CAS marks the
// old carrier with `UPDATE_MARK` AND links a same-key replacement after it, so
// the key is NEVER absent; the old node's teardown reuses the deferred
// physical-delete machinery (`linked_height` handshake) verbatim. The value is
// INLINE in the node — no pointer indirection on reads.
//
// History: the ORIGINAL node-replacement UPDATE here used lock-free HELPING
// and was removed: that variant could not guarantee the old node was
// unreachable before it was retired — a use-after-free under real EpochGuard
// reclamation (reproduced with ASan). The restored design needs no helping FOR
// the update itself (helping flows the other way: traversal snips preserve the
// splice automatically, and a still-linking inserter inherits the teardown via
// the handshake); unreachability-before-retire now rests on the same two
// pillars as delete — tower quiescence (handshake) + the strictly-greater-key
// unlink walk in `unlink_at_level` that tolerates equal-key zombie runs.
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
/// - Key data (uninit for sentinel)
/// - Forward pointers (next) at each level, with mark bits for deletion
///
/// height_flags layout: bits 0-5 = height (max 63), bit 7 = sentinel flag
///
/// `linked_height` flag bit: a delete won the level-0 mark while the tower was
/// (possibly) still being linked. Set by the deleter's `fetch_or`; observed by
/// the inserter's publication `swap`, which then runs the deferred physical
/// delete phase. Distinct from any height value (`MAX_LEVEL = 32 < 0x40`).
const DEL_PENDING: u8 = 0x40;

#[repr(C)]
pub struct SkipNode<T> {
    key: MaybeUninit<T>,
    height_flags: u8,
    /// Tower-quiescence handshake word — NOT a visibility bit (visibility is the
    /// level-0 mark alone; see [`SkipNode::is_visible`]).
    ///
    /// `0` at allocation. The inserter publishes tower quiescence with ONE
    /// `swap(height)` (AcqRel) after its linking loop ends — including when the
    /// loop aborts early because a concurrent delete marked level 0. A deleter
    /// that wins the level-0 mark does `fetch_or(DEL_PENDING)` (AcqRel). The
    /// single modification order of this atomic arbitrates the physical delete
    /// phase: whichever RMW observes the other's value runs `physical_delete`
    /// exactly once (see the protocol block at the top of this file). Reading
    /// `height` here proves the linking loop is done — no insert CAS or plain
    /// tower store for this node is or will be in flight — and it never
    /// regresses, unlike top-level reachability, which a concurrent delete
    /// transiently breaks (the gate that once made delete spuriously fail for a present key).
    linked_height: AtomicU8,
    // Flexible array: pointers are allocated inline after this struct
    // Layout: [next[0], next[1], ..., next[h-1]]
    // Total: height pointers
    pointers: [AtomicPtr<SkipNode<T>>; 0],
}

const SENTINEL_FLAG: u8 = 0x80;

/// Head sentinel node with fixed MAX_LEVEL tower, embedded inline in SkipList.
///
/// Layout-compatible with SkipNode<T> when cast via pointer:
/// same #[repr(C)] prefix (key, height_flags) followed by the pointer array.
/// The only difference is the pointer array has a fixed size of MAX_LEVEL
/// instead of being a flexible array member.
#[repr(C)]
struct HeadNode<T> {
    key: MaybeUninit<T>,
    height_flags: u8,
    linked_height: AtomicU8,
    pointers: [AtomicPtr<SkipNode<T>>; MAX_LEVEL],
}

impl<T> HeadNode<T> {
    fn new() -> Self {
        HeadNode {
            key: MaybeUninit::uninit(),
            height_flags: MAX_LEVEL as u8 | SENTINEL_FLAG,
            linked_height: AtomicU8::new(MAX_LEVEL as u8),
            pointers: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
        }
    }
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
        // SAFETY: Layout is computed from the node struct + flexible array of `height`
        // AtomicPtrs. `alloc` returns a valid, aligned, non-null pointer (or we abort
        // via `handle_alloc_error`). We field-initialize via `addr_of_mut!` + `write`
        // to avoid creating an intermediate reference to uninitialized memory, and
        // zero-initialize the pointer array to null (valid for AtomicPtr).
        unsafe {
            let layout = Self::get_layout(height);
            let ptr = alloc(layout) as *mut Self;
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }

            // Initialize fields
            ptr::addr_of_mut!((*ptr).key).write(MaybeUninit::new(key));
            ptr::addr_of_mut!((*ptr).height_flags).write(height as u8);
            // 0 = tower not yet quiescent: the inserter's single `swap(height)`
            // publication (after its linking loop) is what raises it. Must NOT
            // start at `height` even for height-1 nodes — a deleter's `fetch_or`
            // reading `height` claims the physical phase, and before the swap the
            // inserter would claim it again via DEL_PENDING (double retire).
            ptr::addr_of_mut!((*ptr).linked_height).write(AtomicU8::new(0));

            // Initialize all pointers to null
            ptr::addr_of_mut!((*ptr).pointers)
                .cast::<AtomicPtr<Self>>()
                .write_bytes(0, height);

            ptr
        }
    }

    /// Deallocate a node, dropping the key value.
    ///
    /// # Safety
    /// - The pointer must have been allocated by `alloc_with_key`.
    /// - The key must not have been moved out (e.g. via `take_value_unlinked`).
    ///   Use `dealloc_node_no_drop` if the key was already moved out.
    unsafe fn dealloc_node(ptr: *mut Self) {
        // SAFETY: Caller guarantees `ptr` was allocated by `alloc_with_key` and the
        // key has not been moved out. We reconstruct the same Layout from the stored
        // height, drop the key (if not a sentinel), then dealloc with the matching layout.
        unsafe {
            let hf = (*ptr).height_flags;
            let height = (hf & !SENTINEL_FLAG) as usize;
            let layout = Self::get_layout(height);

            // Drop the key if not a sentinel
            if hf & SENTINEL_FLAG == 0 {
                ptr::addr_of_mut!((*ptr).key).cast::<T>().drop_in_place();
            }

            dealloc(ptr as *mut u8, layout);
        }
    }

    /// Deallocate a node without dropping the key value.
    ///
    /// # Safety
    /// - The pointer must have been allocated by `alloc_with_key`.
    /// - The key must have already been moved out (e.g. via `take_value_unlinked`).
    unsafe fn dealloc_node_no_drop(ptr: *mut Self) {
        // SAFETY: Caller guarantees `ptr` was allocated by `alloc_with_key` and the key
        // was already moved out (e.g. via `take_value_unlinked`). We reconstruct the
        // matching Layout from the stored height and dealloc without dropping the key.
        unsafe {
            let height = ((*ptr).height_flags & !SENTINEL_FLAG) as usize;
            dealloc(ptr as *mut u8, Self::get_layout(height));
        }
    }

    #[inline(always)]
    fn is_sentinel(&self) -> bool {
        self.height_flags & SENTINEL_FLAG != 0
    }

    /// Get height
    #[inline(always)]
    fn height(&self) -> usize {
        (self.height_flags & !SENTINEL_FLAG) as usize
    }

    /// Take ownership of the value from this node.
    ///
    /// # Safety
    /// - Must only be called on an unlinked node (never inserted into list)
    /// - Must only be called once
    /// - Node must not be a sentinel
    ///
    /// This is an internal method used for CAS retry recovery, NOT exposed
    /// through the CollectionNode trait (which would be unsafe in concurrent contexts).
    unsafe fn take_value_unlinked(&mut self) -> T {
        // SAFETY: Caller guarantees the node was initialized by `alloc_with_key`
        // (so `key` is init), is not a sentinel, was never published to the list,
        // and this is called only once, so no double-read of the MaybeUninit.
        unsafe { self.key.assume_init_read() }
    }

    /// THE visibility predicate: a node is a *visible member* of the list
    /// iff its level-0 next pointer is unmarked — i.e. it has been published by the
    /// level-0 CAS and not logically deleted. A node whose tower is still being
    /// linked (`linked_height < height`) IS a member: the record exists the moment
    /// the level-0 CAS lands; upper levels are a routing accelerator, not presence.
    ///
    /// Every operation that answers "is this key present?" — `contains`/`find`
    /// (`find_node_internal`), exact-match `find_from_internal`, the iteration
    /// helpers, and insert's duplicate decision — MUST use this one predicate.
    /// Per-call-site ad-hoc gates are exactly what produced non-linearizable
    /// histories like `insert(k) -> "exists"` followed by `get(k) -> None` on the
    /// same thread — non-linearizable). Crucially, this predicate must NOT include tower
    /// publication: gating presence on `linked_height` forces insert to WAIT for a
    /// concurrent inserter's linking loop (an unbounded spin on another thread's
    /// progress — not lock-free). With level-0-only visibility, insert fails
    /// immediately on a physical duplicate and every read agrees it is present.
    ///
    /// Linearization points under this predicate:
    /// - insert: its successful level-0 publishing CAS;
    /// - delete: its level-0 DELETE-mark CAS (see `mark_and_unlink`);
    /// - reads:  evaluation of this predicate on the candidate node.
    ///
    /// `linked_height` plays no part in visibility; it is the tower-quiescence
    /// handshake word for the deferred PHYSICAL delete phase (see the protocol
    /// block at the top of this file).
    ///
    /// # Safety
    /// `node` must be a valid, non-null, guard-protected node pointer.
    #[inline(always)]
    unsafe fn is_visible(node: *mut Self) -> bool {
        // SAFETY: caller guarantees `node` validity; we read one atomic tower slot
        // (level 0 always exists).
        unsafe { !MarkedPtr::new(Self::get_next(node, 0)).is_any_marked() }
    }

    /// Resolve a key-matching candidate to the LIVE CARRIER of its key by
    /// following the UPDATE-replacement chain: an UPDATE-marked node's level-0
    /// next pointer is its same-key replacement (spliced in by the update's
    /// linearizing CAS — see `update_internal`). Returns
    /// `Some(carrier)` when the chain ends in an unmarked node (the key is
    /// present, carried by that node) or `None` when it ends in a DELETE mark
    /// (the key was removed).
    ///
    /// Lock-free: every hop follows another thread's already-linearized splice,
    /// and chains are finite (each link is a completed update; the final node is
    /// either live or deleted).
    ///
    /// This is the companion of [`Self::is_visible`]: `is_visible` says whether
    /// THIS node is the live carrier; `resolve_replacement` answers the question
    /// reads actually ask — "is the KEY present, and which node carries it?"
    ///
    /// # Safety
    /// `node` must be a valid, non-null, guard-protected, non-sentinel node; the
    /// guard pin must cover the whole call (chain nodes are kept alive by it).
    unsafe fn resolve_replacement(mut node: *mut Self) -> Option<*mut Self> {
        loop {
            // SAFETY: `node` starts valid per contract; each hop target was
            // published by a splice CAS (Release) and is guard-protected.
            let marked = MarkedPtr::new(unsafe { Self::get_next(node, 0) });
            if !marked.is_any_marked() {
                return Some(node);
            }
            if marked.is_delete_marked() {
                return None;
            }
            // UPDATE-marked: follow to the same-key replacement.
            node = marked.as_ptr();
        }
    }

    // =========================================================================
    // Pointer access helpers
    // =========================================================================

    /// Raw pointer to the AtomicPtr slot at `index` in the flexible tower array.
    ///
    /// Takes a RAW node pointer and derives the slot via `addr_of_mut!` — never
    /// through `&SkipNode<T>`/`&self`. The tower lives PAST
    /// `size_of::<SkipNode<T>>()` in the same allocation, so a reference to the node
    /// has no provenance over it: any `&self`-based tower access is out-of-bounds of
    /// the reference under Stacked/Tree Borrows (Miri-flagged UB), even where codegen
    /// happens to be benign. ALL tower traffic must go through these raw accessors;
    /// `&self` methods remain fine for in-struct fields (`key`, `height_flags`,
    /// `linked_height`), which lie within the reference's extent.
    ///
    /// # Safety
    /// - `node` must point to a live `alloc_with_key` allocation (or the inline
    ///   `HeadNode`, which is layout-compatible with a MAX_LEVEL tower).
    /// - `index < height(node)`.
    #[inline(always)]
    unsafe fn tower_slot(node: *mut Self, index: usize) -> *const AtomicPtr<SkipNode<T>> {
        // SAFETY: caller guarantees `node` validity and `index < height`; the slot
        // lies within the node's single allocation and was initialized by
        // `alloc_with_key` / `HeadNode::new`. `addr_of_mut!` keeps the original
        // (whole-allocation) provenance of `node`.
        unsafe {
            debug_assert!(
                index < (*node).height(),
                "level {index} >= height {}",
                (*node).height()
            );
            ptr::addr_of_mut!((*node).pointers)
                .cast::<AtomicPtr<SkipNode<T>>>()
                .add(index)
        }
    }

    // =========================================================================
    // Next pointer accessors (indices 0..height) — raw-pointer based, see
    // `tower_slot` for the provenance rationale.
    // =========================================================================

    /// Load next pointer at level (consume ordering).
    ///
    /// Uses load_consume: Relaxed + compiler_fence on ARM (plain `ldr`),
    /// Acquire on x86 (plain `mov` under TSO). Safe for pointer-chasing
    /// traversals where data-dependency ordering suffices.
    ///
    /// # Safety
    /// `node` must be a valid, live (guard-protected or owned) node pointer with
    /// `level < height(node)`.
    #[inline(always)]
    unsafe fn get_next(node: *mut Self, level: usize) -> *mut SkipNode<T> {
        // SAFETY: slot derived with whole-allocation provenance; a shared reference
        // to ONE in-bounds, initialized AtomicPtr is created only for the atomic op.
        unsafe { load_consume(&*Self::tower_slot(node, level)) }
    }

    /// Store next pointer at level (Relaxed ordering)
    /// Used for initial setup of unpublished nodes before CAS-linking.
    /// Safe because the CAS that publishes the node provides Release semantics.
    ///
    /// # Safety
    /// Same as [`Self::get_next`]; additionally the store must not race readers in a
    /// way the protocol forbids (only used on unpublished levels).
    #[inline(always)]
    unsafe fn set_next_relaxed(node: *mut Self, level: usize, ptr: *mut SkipNode<T>) {
        // SAFETY: see `tower_slot`; in-bounds initialized AtomicPtr.
        unsafe { (*Self::tower_slot(node, level)).store(ptr, Ordering::Relaxed) }
    }

    /// CAS next pointer at level (Release/Acquire ordering)
    ///
    /// # Safety
    /// Same as [`Self::get_next`].
    #[inline(always)]
    unsafe fn cas_next(
        node: *mut Self,
        level: usize,
        expected: *mut SkipNode<T>,
        new: *mut SkipNode<T>,
    ) -> Result<*mut SkipNode<T>, *mut SkipNode<T>> {
        // SAFETY: see `tower_slot`; the CAS itself is atomic and cannot cause UB.
        // Acquire on failure ensures that when callers (e.g. `unlink_at_level`)
        // dereference the returned pointer (is_sentinel, key comparison), they
        // observe the stores made by the thread that published that node via a
        // Release CAS. Without Acquire, this is unsound on weakly-ordered
        // architectures (ARM/AArch64).
        unsafe {
            (*Self::tower_slot(node, level)).compare_exchange(
                expected,
                new,
                Ordering::Release,
                Ordering::Acquire,
            )
        }
    }

    /// Weak CAS next pointer at level (Release/Acquire ordering)
    ///
    /// # Safety
    /// Same as [`Self::get_next`].
    #[inline(always)]
    unsafe fn cas_next_weak(
        node: *mut Self,
        level: usize,
        expected: *mut SkipNode<T>,
        new: *mut SkipNode<T>,
    ) -> Result<*mut SkipNode<T>, *mut SkipNode<T>> {
        // SAFETY: see `tower_slot`; Acquire on failure — same rationale as
        // `cas_next` above.
        unsafe {
            (*Self::tower_slot(node, level)).compare_exchange_weak(
                expected,
                new,
                Ordering::Release,
                Ordering::Acquire,
            )
        }
    }
}

impl<T> CollectionNode<T> for SkipNode<T> {
    #[inline(always)]
    fn key(&self) -> &T {
        // SAFETY: All non-sentinel nodes are initialized by `alloc_with_key`, which
        // writes a valid `T` into `key`. Callers must not invoke this on sentinels
        // (typically guarded by short-circuit checks like `is_sentinel() || ...`).
        // The reference borrows `self`, so it cannot outlive the node.
        unsafe { self.key.assume_init_ref() }
    }

    /// Deallocate using custom allocator (flexible array member pattern).
    ///
    /// SkipNode uses a custom layout with the allocator API for its flexible
    /// array member (pointers), so we must use the matching deallocation.
    ///
    unsafe fn dealloc_ptr(ptr: *mut Self) {
        // SAFETY: Caller guarantees `ptr` was allocated by `alloc_with_key`, the node
        // is fully unlinked, and the key has not been moved out. Delegates to
        // `dealloc_node` which drops the key and frees with the matching layout.
        unsafe {
            Self::dealloc_node(ptr);
        }
    }
}

impl<K: Eq, V> MapNode<K, V> for SkipNode<Pair<K, V>> {
    #[inline]
    fn key(&self) -> &K {
        &CollectionNode::key(self).key
    }

    #[inline]
    fn value(&self) -> Option<&V> {
        Some(&CollectionNode::key(self).value)
    }

    /// SkipNode uses flexible-array allocation — NOT Box.
    unsafe fn dealloc_ptr(ptr: *mut Self) {
        // SAFETY: Same preconditions as `CollectionNode::dealloc_ptr` — caller
        // guarantees the pointer was allocated by `alloc_with_key`, node is unlinked,
        // and key has not been moved out.
        unsafe { Self::dealloc_node(ptr) }
    }
}

impl<K: Eq, V> MapNode<K, V> for SkipNode<MapEntry<K, V>> {
    #[inline]
    fn key(&self) -> &K {
        CollectionNode::key(self).key()
    }

    #[inline]
    fn value(&self) -> Option<&V> {
        // `None` for a claimed (tombstoned) entry — logically removed (see MapEntry).
        let p = CollectionNode::key(self).value_ptr();
        if p.is_null() {
            return None;
        }
        // SAFETY: non-null observed above; callers hold a pinned guard (BUG-9 caller
        // obligation), so a concurrently-replaced/claimed+retired value is kept alive
        // for this borrow.
        Some(unsafe { &*p })
    }

    /// SkipNode uses flexible-array allocation — NOT Box. Dropping the entry frees
    /// its current value (see `MapEntry`'s `Drop`).
    unsafe fn dealloc_ptr(ptr: *mut Self) {
        // SAFETY: Same preconditions as `CollectionNode::dealloc_ptr`.
        unsafe { Self::dealloc_node(ptr) }
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
    /// Number of valid preds entries (0..valid_height are valid).
    /// Entries at or above this index may be uninitialized.
    valid_height: usize,
}

// No Copy/Clone: positions hold raw pointers and must not outlive
// the guard epoch in which they were created (move-only).

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
            valid_height: 0,
        }
    }

    fn from_node(node: *mut Self::Node) -> Self {
        SkipNodePosition {
            preds: [ptr::null_mut(); MAX_LEVEL],
            node,
            valid_height: 0,
        }
    }

    fn is_valid(&self) -> bool {
        !self.node.is_null()
    }
}

impl<T> SkipNodePosition<T> {
    /// Create a new position with predecessors and node
    pub fn new(
        preds: [SkipNodePtr<T>; MAX_LEVEL],
        node: SkipNodePtr<T>,
        valid_height: usize,
    ) -> Self {
        SkipNodePosition {
            preds,
            node,
            valid_height,
        }
    }

    /// Get the predecessors array and valid height
    pub fn preds(&self) -> (&[SkipNodePtr<T>; MAX_LEVEL], usize) {
        (&self.preds, self.valid_height)
    }
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
pub struct SkipList<T, G: Guard> {
    /// Head sentinel with fixed MAX_LEVEL tower, embedded inline to avoid
    /// one heap allocation and one pointer dereference per traversal.
    head_node: HeadNode<T>,
    /// Highest level currently in use (1-indexed). Starts at 1 for empty list.
    /// Relaxed ordering: this is a hint for traversal optimization.
    /// Under-reading is safe (just starts higher), over-reading is impossible
    /// because we only ever increase it.
    current_height: AtomicUsize,
    /// Shared guard instance for deferred destruction.
    /// All deleted nodes are deferred to this guard and freed when it drops.
    guard: G,
    /// Element count for the MAP layers' O(1) `len_internal` ONLY — maintained at
    /// the map linearization points (`MapEntry`: publish/claim; `Pair`: publish/
    /// mark-win), NEVER on the shared set path: a counter there is a single
    /// contended cache line that serializes otherwise-disjoint concurrent inserts
    /// (measured 4x anti-scaling at 32 threads). The set's `SortedCollection::len`
    /// walks the list — O(n) by design. Relaxed ordering: may be transiently stale
    /// under concurrent modification, eventually consistent.
    size: AtomicUsize,
}

impl<T, G: Guard> SkipList<T, G> {
    /// Get head sentinel as a SkipNode pointer.
    ///
    /// Safe because HeadNode has the same #[repr(C)] prefix layout as SkipNode,
    /// and the fixed-size pointer array occupies the same offset as SkipNode's
    /// flexible array member.
    #[inline(always)]
    fn head(&self) -> *mut SkipNode<T> {
        &self.head_node as *const HeadNode<T> as *mut SkipNode<T>
    }
}

impl<T: Ord, G: Guard> SkipList<T, G> {
    /// Create a new empty skip list
    pub fn new() -> Self {
        SkipList {
            head_node: HeadNode::new(),
            current_height: AtomicUsize::new(1),
            guard: G::default(),
            size: AtomicUsize::new(0),
        }
    }

    /// Get the shared guard instance for this collection.
    /// Used by SortedCollection trait methods for deferred destruction.
    pub fn guard(&self) -> &G {
        &self.guard
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
    ///
    /// Height clamping: prevents nodes from being much taller than the current
    /// list. If level `height-2` of the head sentinel is empty (no nodes at
    /// that level), reduce height. This ensures the list grows gradually and
    /// avoids sparse upper levels that hurt traversal performance.
    #[inline]
    fn random_level(&self) -> usize {
        // Generate random bits and count trailing ones
        // Each trailing 1 bit adds one level (with 50% probability each)
        let random_bits = fastrand::u32(..);

        // Count trailing ones (equivalent to counting consecutive "heads" in coin flips)
        // trailing_ones() counts how many 1s before the first 0
        let extra_levels = (!random_bits).trailing_zeros() as usize;

        // Height clamping: grow at most one level at a time, capped at MAX_LEVEL.
        let max = (self.current_height.load(Ordering::Relaxed) + 1).min(MAX_LEVEL);
        (1 + extra_levels).min(max)
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
    #[cold]
    #[inline(never)]
    fn recover_pred(&self, level: usize, preds: &[SkipNodePtr<T>]) -> *mut SkipNode<T> {
        // Try each higher level until we find an unmarked pred or reach HEAD
        for &pred in preds.iter().skip(level + 1) {
            if pred.is_null() {
                break;
            }

            // Skip HEAD - it's always valid, use it directly if we reach here
            if pred == self.head() {
                return self.head();
            }

            // Check if this pred is logically deleted (next[0] is marked)
            // SAFETY: `pred` comes from the preds array, populated during a prior
            // traversal while a guard was pinned. The guard prevents
            // the node from being freed while it is held, so dereferencing
            // is valid. We only read atomic fields (next[0], height).
            unsafe {
                let pred_next_0 = SkipNode::get_next(pred, 0);
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
        self.head()
    }

    /// Handle a marked node encountered during traversal.
    ///
    /// `next` is the raw (possibly marked) pointer loaded from `curr.next[level]`.
    /// Tries to snip the marked `curr` out of the list at `level`.
    /// On CAS failure, recovers pred from higher levels.
    /// Returns (new_pred, new_curr) for the caller to continue with.
    #[cold]
    #[inline(never)]
    unsafe fn snip_marked_node(
        &self,
        level: usize,
        pred: SkipNodePtr<T>,
        curr: SkipNodePtr<T>,
        next: SkipNodePtr<T>,
        preds: &[SkipNodePtr<T>],
    ) -> (SkipNodePtr<T>, SkipNodePtr<T>) {
        // SAFETY: Caller guarantees `pred` and `curr` are valid, non-null node
        // pointers protected by the memory reclamation guard. `level < pred.height()`
        // is ensured by `recover_pred`'s height check. `cas_next` on pred is an atomic
        // CAS on a valid AtomicPtr. On failure, `recover_pred` returns a valid predecessor.
        unsafe {
            let clean_next = MarkedPtr::unmask(next);
            match SkipNode::cas_next(pred, level, curr, clean_next) {
                Ok(_) => (pred, clean_next),
                Err(_) => {
                    let new_pred = self.recover_pred(level, preds);
                    let new_curr = MarkedPtr::new(SkipNode::get_next(new_pred, level)).as_ptr();
                    (new_pred, new_curr)
                }
            }
        }
    }

    /// Lightweight key lookup — `find_position` minus the succs array.
    ///
    /// Same traversal and recovery logic as `find_position` (preds array,
    /// `recover_pred`, `snip_marked_node`), but only tracks the level-0
    /// candidate instead of recording successors at every level.
    ///
    /// # Precondition
    ///
    /// The caller **must** hold a pinned epoch guard (`G::pin()`) for the
    /// duration of this call. Without it, concurrent reclamation may free
    /// nodes that this traversal is still reading.
    #[inline]
    pub(crate) fn search_key_internal<Q>(&self, key: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        self.find_node_internal(key).is_some()
    }

    /// Like `search_key_internal`, but returns the node pointer instead of bool.
    ///
    /// Returns `Some(ptr)` if a VISIBLE node (level-0 linked, unmarked — see
    /// [`SkipNode::is_visible`]) with matching key exists, `None` otherwise.
    ///
    /// # Precondition
    ///
    /// The caller **must** hold a pinned epoch guard (`G::pin()`).
    pub(crate) fn find_node_internal<Q>(&self, key: &Q) -> Option<*mut SkipNode<T>>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let mut preds: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
        let head = self.head();
        let mut pred = head;
        let top = self.current_height.load(Ordering::Relaxed);
        debug_assert!(top <= MAX_LEVEL);
        if top < MAX_LEVEL {
            preds[top] = ptr::null_mut();
        }
        let mut curr: *mut SkipNode<T> = std::ptr::null_mut();

        for level in (0..top).rev() {
            // SAFETY: `pred` is HEAD or a traversal-found node, guard-protected.
            // Reading its atomic next pointer is safe.
            curr = unsafe {
                let pred_next = SkipNode::get_next(pred, level);
                if MarkedPtr::new(pred_next).is_any_marked() {
                    pred = self.recover_pred(level, &preds);
                    MarkedPtr::new(SkipNode::get_next(pred, level)).as_ptr()
                } else {
                    pred_next
                }
            };

            while !curr.is_null() {
                // SAFETY: `curr` is non-null (loop guard), loaded from a live node's
                // atomic next pointer, and guard-protected against reclamation.
                unsafe {
                    let next = SkipNode::get_next(curr, level);

                    // Prefetch next node's cache line while we compare curr's key.
                    // Overlaps memory latency with the comparison work below.
                    prefetch_read(next);

                    if MarkedPtr::new(next).is_any_marked() {
                        (pred, curr) = self.snip_marked_node(level, pred, curr, next, &preds);
                        continue;
                    }

                    if (*curr).key().borrow() < key {
                        pred = curr;
                        curr = next;
                    } else {
                        break;
                    }
                }
            }

            preds[level] = pred;
        }

        // SAFETY: `curr` is non-null (short-circuit `&&` below checks `is_null` first),
        // guard-protected, found at level 0. Reading key and next[0] is safe on a
        // valid node. Presence is decided by the ONE shared resolution
        // (`resolve_replacement` over the level-0 mark) so contains/find/insert/
        // delete/iteration agree (the one-predicate rule): an UPDATE-marked candidate resolves
        // to its same-key replacement (present), a DELETE-marked one to absent.
        if !curr.is_null() && unsafe { (*curr).key().borrow() == key } {
            unsafe { SkipNode::resolve_replacement(curr) }
        } else {
            None
        }
    }

    /// Link a new node at a specific level.
    ///
    /// Tries to CAS pred.next[level] from expected_succ to new_node.
    /// new_node.next[level] should already be set to expected_succ.
    ///
    /// Returns Ok(()) on success, Err(actual) on CAS failure.
    #[inline]
    #[allow(dead_code)]
    unsafe fn link_at_level(
        &self,
        level: usize,
        pred: SkipNodePtr<T>,
        new_node: SkipNodePtr<T>,
        expected_succ: SkipNodePtr<T>,
    ) -> Result<(), *mut SkipNode<T>> {
        // SAFETY: Caller guarantees `pred` is a valid, guard-protected node pointer
        // with `height > level`. The CAS is an atomic operation on a valid AtomicPtr.
        unsafe { SkipNode::cas_next(pred, level, expected_succ, new_node).map(|_| ()) }
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
        // SAFETY: Caller guarantees all node pointers (`pred`, `new_node`, preds entries)
        // are valid and guard-protected. `new_node` was allocated by `alloc_with_key` and
        // is already published at level 0. All dereferences read atomic fields (next
        // pointers, keys) on live nodes that cannot be reclaimed while the guard is held.
        unsafe {
            loop {
                // Check if new_node is being deleted/updated at level 0
                let node_next_0 = SkipNode::get_next(new_node, 0);
                if MarkedPtr::new(node_next_0).is_any_marked() {
                    return false; // Node is being removed, stop
                }

                // NOTE: there is deliberately NO level-0 mark check on `pred` or on
                // advance candidates here — only marks AT THIS LEVEL matter. A
                // level-0-marked zombie whose teardown is deferred can stay linked
                // (unmarked) at this level indefinitely; refusing to use it and
                // re-reading was an unbounded wait on another thread's physical
                // phase (a spin lock — not lock-free). Using a zombie as pred is
                // SAFE under the mark-before-unlink discipline: the teardown marks
                // each level before unlinking it, so our link CAS below either
                // fails on the marked slot (we recover), or lands first and the
                // teardown's unlink splice `pred.next: zombie -> unmask(zombie.next)`
                // carries our newly linked node.

                let pred_next = SkipNode::get_next(*pred, level);
                let pred_next_marked = MarkedPtr::new(pred_next);
                let pred_next_ptr = pred_next_marked.as_ptr();

                // If pred.next is marked at THIS level, pred is deleted at this
                // level — recover from level+1
                if pred_next_marked.is_any_marked() {
                    *pred = self.recover_pred(level, preds);
                    continue;
                }

                // Advance pred if needed (concurrent insert of smaller key)
                if !pred_next_ptr.is_null()
                    && !(*pred_next_ptr).is_sentinel()
                    && (*pred_next_ptr).height() > level
                    && (*pred_next_ptr).key() < node_key
                {
                    // If the candidate is deleted AT THIS LEVEL, snip it past
                    // (helping) instead of advancing onto it — advancing would
                    // bounce off its marked slot and recover back here forever.
                    let cand_next = SkipNode::get_next(pred_next_ptr, level);
                    let cand_marked = MarkedPtr::new(cand_next);
                    if cand_marked.is_any_marked() {
                        // Success or failure both mean progress (ours or another
                        // thread's CAS on this slot); re-read either way.
                        let _ = SkipNode::cas_next(
                            *pred,
                            level,
                            pred_next_ptr,
                            MarkedPtr::unmask(cand_next),
                        );
                        continue;
                    }
                    *pred = pred_next_ptr;
                    continue;
                }

                // If pred.next is already new_node, we're done
                if pred_next_ptr == new_node {
                    return true;
                }

                // Relaxed PLAIN store: this level isn't linked yet, so only this
                // inserter writes the slot — no other thread can CAS-mark
                // new_node.next[level] before the publishing CAS below makes it
                // reachable, and upper-level DELETE marks happen only in the
                // physical delete phase, which is gated on tower quiescence (the
                // `linked_height` handshake — it cannot start until this linking
                // loop has finished), so this store can never erase a mark. A
                // LEVEL-0 mark may land mid-loop; the loop-top check aborts
                // linking and the publication swap hands us the teardown.
                SkipNode::set_next_relaxed(new_node, level, pred_next_ptr);

                // CAS pred.next from pred_next_ptr to new_node
                // On failure, retry - re-read pred.next and try again
                match SkipNode::cas_next(*pred, level, pred_next_ptr, new_node) {
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
    ) -> u8 {
        // SAFETY: Caller guarantees `pred`, `node`, and preds entries are valid,
        // guard-protected pointers. `node` has been logically marked at this level.
        // All dereferences read atomic next pointers and immutable keys on live nodes.
        // CAS operations are atomic and do not cause UB on failure.
        unsafe {
            loop {
                let pred_next = SkipNode::get_next(*pred, level);
                let pred_next_marked = MarkedPtr::new(pred_next);
                let pred_next_ptr = pred_next_marked.as_ptr();

                // A MARKED slot means PRED ITSELF is deleted at this level. Its
                // frozen value is a snapshot from pred's own teardown — it may
                // point ANYWHERE relative to `node` (even past it, at a key
                // strictly greater, while `node` is still linked elsewhere).
                // NO conclusion may be drawn from a frozen slot: this check must
                // precede every early-return below. Drawing the "already
                // unlinked" conclusions from a dead pred's frozen slot was a
                // RETIRE-WHILE-REACHABLE use-after-free (release-mode SIGSEGV;
                // caught by `debug_assert_unreachable`, unlink reason 4).
                if pred_next_marked.is_any_marked() {
                    *pred = self.recover_pred(level, preds);
                    continue;
                }

                if pred_next_ptr != node {
                    // pred.next != node (CLEAN slot): either unlinked or a
                    // concurrent insert happened. "Already unlinked" may only be
                    // concluded at a STRICTLY greater key (or end/sentinel). An
                    // EQUAL key is NOT proof: a deleted zombie awaiting its
                    // deferred physical phase can coexist at routing levels with
                    // a re-inserted node of the same key, with `node` linked
                    // BEHIND it (pred -> new(k) -> zombie(k)). Advance THROUGH
                    // equal keys; the run is finite.
                    if pred_next_ptr.is_null() {
                        return 2; // end of level — already unlinked
                    }
                    if (*pred_next_ptr).is_sentinel() {
                        return 3; // sentinel boundary — already unlinked
                    }
                    if (*pred_next_ptr).key() > node_key {
                        return 4; // strictly greater key — already unlinked
                    }
                    // Concurrent insert happened (or an equal-key run) — advance pred.
                    // NEVER advance onto a candidate whose own slot is marked at this
                    // level: the loop-top marked-slot check would bounce us to
                    // recover_pred and back here forever (with no other thread
                    // running, that is a SOLO infinite loop — observed via gdb in
                    // update's physical_delete). Same rule as insert_at_level's
                    // advance: help snip the marked candidate instead; success or
                    // failure both mean progress (ours or another thread's CAS).
                    let cand_next = SkipNode::get_next(pred_next_ptr, level);
                    let cand_marked = MarkedPtr::new(cand_next);
                    if cand_marked.is_any_marked() {
                        let _ = SkipNode::cas_next(
                            *pred,
                            level,
                            pred_next_ptr,
                            MarkedPtr::unmask(cand_next),
                        );
                        continue;
                    }
                    *pred = pred_next_ptr;
                    continue;
                }

                // pred.next == node on a CLEAN slot — unlink it.
                // Compute replacement: use provided or compute from node.next[level]
                let replacement_ptr = replacement.unwrap_or_else(|| {
                    let node_next = SkipNode::get_next(node, level);
                    MarkedPtr::unmask(node_next)
                });

                // Try to unlink: CAS pred.next from node to replacement
                match SkipNode::cas_next(*pred, level, node, replacement_ptr) {
                    Ok(_) => return 1,
                    Err(actual) => {
                        // Same rule as the loop top: a MARKED actual means pred
                        // died under us — its frozen value proves nothing.
                        if MarkedPtr::new(actual).is_any_marked() {
                            *pred = self.recover_pred(level, preds);
                            continue;
                        }
                        let actual_ptr = MarkedPtr::unmask(actual);
                        // Check if replacement is already linked
                        if actual_ptr == replacement_ptr {
                            return 6; // Already unlinked with this replacement
                        }
                        // CLEAN slot conclusions only — strictly-greater key
                        // (equal keys are advanced through, see above).
                        if actual_ptr.is_null() {
                            return 7;
                        }
                        if (*actual_ptr).is_sentinel() {
                            return 8;
                        }
                        if (*actual_ptr).key() > node_key {
                            return 9;
                        }
                        // Same advance rule as above: never step onto a candidate
                        // whose own slot is marked — help snip it instead (the
                        // recover/advance bounce is a solo infinite loop).
                        let cand_next = SkipNode::get_next(actual_ptr, level);
                        let cand_marked = MarkedPtr::new(cand_next);
                        if cand_marked.is_any_marked() {
                            let _ = SkipNode::cas_next(
                                *pred,
                                level,
                                actual_ptr,
                                MarkedPtr::unmask(cand_next),
                            );
                            continue;
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
    /// Monolithic find_position: inlines the per-level traversal directly to give
    /// the compiler maximum optimisation room (single function, no call overhead,
    /// shared register allocation across all levels).
    /// Not marked #[inline] — large function called from insert/remove/find;
    /// let the compiler decide (avoids icache bloat from duplication).
    fn find_position<Q>(
        &self,
        key: &Q,
        max_height: usize,
        start_preds: Option<(&[SkipNodePtr<T>; MAX_LEVEL], usize)>,
        preds: &mut [SkipNodePtr<T>; MAX_LEVEL],
        succs: &mut [SkipNodePtr<T>; MAX_LEVEL],
    ) -> usize
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let head = self.head();
        let mut last_pred = head;

        // Start from current_height (Relaxed hint) instead of MAX_LEVEL.
        // Skips empty upper levels. Under-reading is safe (starts higher than needed).
        let top = self.current_height.load(Ordering::Relaxed).max(max_height);
        debug_assert!(top <= MAX_LEVEL);

        // Sentinel for recover_pred: stops upward scan above the traversed range.
        // When top == MAX_LEVEL (max_height callers), recover_pred at the highest
        // level gets an empty iterator (no entries above MAX_LEVEL-1), so no
        // sentinel is needed — all preds[0..MAX_LEVEL-1] are written by the loop.
        if top < MAX_LEVEL {
            preds[top] = ptr::null_mut();
        }

        for level in (0..top).rev() {
            // Try to use provided predecessor at this level for O(1) batch optimization
            if let Some((hint_preds, valid_height)) = start_preds
                && level < valid_height
            {
                let hint_pred = hint_preds[level];
                // SAFETY: `hint_pred` is non-null (checked) and comes from a
                // prior `find_position` preds array, guard-protected. Height is
                // checked before `get_next` to avoid out-of-bounds on the
                // flexible array member. We only read atomic fields (next) and
                // immutable key to validate it.
                if !hint_pred.is_null() {
                    unsafe {
                        if (*hint_pred).height() > level {
                            let hint_next = SkipNode::get_next(hint_pred, level);
                            let is_marked = MarkedPtr::new(hint_next).is_any_marked();
                            let key_ok =
                                (*hint_pred).is_sentinel() || (*hint_pred).key().borrow() < key;
                            if !is_marked && key_ok {
                                last_pred = hint_pred;
                            }
                        }
                    }
                }
            }

            // SAFETY (debug_assert only): `last_pred` is HEAD or a validated hint
            // node, both guard-protected. Reading `height` is safe on a valid node.
            debug_assert!(
                unsafe { (*last_pred).height() } > level,
                "INVARIANT VIOLATION: last_pred height {} <= level {}",
                unsafe { (*last_pred).height() },
                level,
            );

            // ── inlined find_at_level ──────────────────────────────────────
            let mut pred = last_pred;

            // Load pred.next[level]. If marked, recover and reload.
            // SAFETY: `pred` is HEAD or a validated node from a higher level,
            // guard-protected. Reading its atomic next pointer is safe.
            let mut curr = unsafe {
                let pred_next = SkipNode::get_next(pred, level);
                if MarkedPtr::new(pred_next).is_any_marked() {
                    pred = self.recover_pred(level, preds);
                    MarkedPtr::new(SkipNode::get_next(pred, level)).as_ptr()
                } else {
                    pred_next
                }
            };

            // Horizontal traversal at this level
            while !curr.is_null() {
                // SAFETY: `curr` is non-null (loop guard), loaded from an atomic
                // next pointer of a live node, and guard-protected against reclamation.
                unsafe {
                    let next = SkipNode::get_next(curr, level);

                    // Prefetch next node's cache line while we compare curr's key.
                    // Overlaps memory latency with the comparison work below.
                    prefetch_read(next);

                    if MarkedPtr::new(next).is_any_marked() {
                        // Cold path: marked node — snip or recover
                        (pred, curr) = self.snip_marked_node(level, pred, curr, next, preds);
                        continue;
                    }

                    debug_assert!(
                        !(*curr).is_sentinel(),
                        "INVARIANT VIOLATION: sentinel as curr at level={}",
                        level,
                    );

                    if (*curr).key().borrow() < key {
                        pred = curr;
                        curr = next; // marks are zero on this path
                    } else {
                        break;
                    }
                }
            }
            // ── end inlined find_at_level ──────────────────────────────────

            last_pred = pred;
            preds[level] = pred;

            if level < max_height {
                succs[level] = curr;
            }
        }

        top
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
        start_preds: Option<(&[SkipNodePtr<T>; MAX_LEVEL], usize)>,
        out_preds: &mut [SkipNodePtr<T>; MAX_LEVEL],
    ) -> (Result<*mut SkipNode<T>, T>, usize) {
        // Delegate to insert_internal_with_height with random height
        // NOTE: `size` is deliberately NOT maintained here. A counter on the shared
        // insert path is a single contended cache line that serializes otherwise
        // disjoint concurrent inserts (measured 4x anti-scaling on the set-insert
        // path at 32 threads). Only the MAP layers count (their `len_internal` is
        // O(1)); the set's `SortedCollection::len` walks — O(n) by design.
        self.insert_internal_with_height(key, self.random_level(), start_preds, out_preds)
    }

    /// Internal insert implementation with specified height.
    /// Used for sentinel nodes that need MAX_LEVEL height.
    /// Also used by insert_internal with random height.
    ///
    /// Returns `Ok(node)` on success (linearizes at the level-0 publishing CAS), or
    /// `Err(key)` when an UNMARKED node with an equal key is physically present at
    /// level 0 — a visible member, even if its tower is still being linked. The key
    /// is handed back so the map layer can distinguish a live duplicate (genuine
    /// failure) from a claimed tombstone (help finish its delete, then retry).
    /// Marked duplicates never produce `Err`: the retry's traversal snips them.
    fn insert_internal_with_height(
        &self,
        mut key: T,
        height: usize,
        start_preds: Option<(&[SkipNodePtr<T>; MAX_LEVEL], usize)>,
        out_preds: &mut [SkipNodePtr<T>; MAX_LEVEL],
    ) -> (Result<*mut SkipNode<T>, T>, usize) {
        'retry: loop {
            // Find position at all levels, using start preds if available
            // SAFETY: succs entries are only read at levels 0..height-1 below,
            // and find_position writes succs[level] for all level < height.
            let mut succs: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
            let top = self.find_position(&key, height, start_preds, out_preds, &mut succs);

            // Check for duplicates at level 0
            if !succs[0].is_null() {
                // SAFETY: `succs[0]` is non-null (just checked), set by `find_position`
                // during a guard-protected traversal. Reading sentinel flag and key
                // are safe on a valid, non-reclaimed node.
                unsafe {
                    if (*succs[0]).is_sentinel() {
                        continue 'retry;
                    }
                    if (*succs[0]).key() == &key {
                        // Duplicate decision must agree with the read resolution
                        // (the one-predicate rule): an UNMARKED physical duplicate is a member —
                        // even one whose tower is still being linked — and an
                        // UPDATE-marked one resolves to its live same-key
                        // replacement, so the insert fails immediately in both
                        // cases. No waiting on the blocker's inserter (that wait
                        // is an unbounded spin on another thread's progress — not
                        // lock-free).
                        if SkipNode::resolve_replacement(succs[0]).is_some() {
                            return (Err(key), top);
                        }
                        // Replacement chain ends DELETE-marked: logically gone,
                        // so it must not fail this insert, and inserting next to
                        // it would duplicate the key at level 0. Re-traverse —
                        // the traversal itself snips marked nodes (helping, not
                        // waiting): each retry follows either our own successful
                        // snip CAS or another thread's completed modification.
                        continue 'retry;
                    }
                }
            }

            // Create new node with specified height
            let new_node = SkipNode::alloc_with_key(key, height);

            // SAFETY: `new_node` was just allocated by `alloc_with_key` — valid and
            // exclusively owned until the publishing CAS. `out_preds` entries are
            // guard-protected nodes from `find_position`. On CAS failure, `new_node`
            // is still exclusively ours (never published), so `take_value_unlinked`
            // and `dealloc_node_no_drop` are safe. On CAS success, `new_node` is
            // published at level 0; subsequent `insert_at_level` calls link higher
            // levels using guard-protected predecessors.
            unsafe {
                // Relaxed: node is unpublished; the CAS below provides Release.
                for (level, &succ) in succs.iter().enumerate().take(height) {
                    SkipNode::set_next_relaxed(new_node, level, succ);
                }

                let result = SkipNode::cas_next(out_preds[0], 0, succs[0], new_node);

                if result.is_err() {
                    // Safe: node was never linked, we have exclusive ownership
                    key = (*new_node).take_value_unlinked();
                    SkipNode::dealloc_node_no_drop(new_node);
                    continue 'retry;
                }

                let node_key = (*new_node).key();

                for level in 1..height {
                    let mut pred = out_preds[level];
                    if !self.insert_at_level(level, &mut pred, new_node, node_key, out_preds) {
                        break;
                    }
                    out_preds[level] = pred;
                }

                // TOWER-QUIESCENCE PUBLICATION (NOT the linearization point — the
                // insert linearized at the level-0 CAS above). One swap, performed
                // even when the linking loop broke early (a concurrent delete
                // marked level 0 — the only reason insert_at_level returns false):
                // "loop done" is exactly what the physical delete phase needs.
                //
                // Handshake (see the protocol block at the top of the file): a
                // deleter that wins the level-0 mark does
                // `fetch_or(DEL_PENDING)` on this same atomic. Its single
                // modification order arbitrates: if our swap returns the flag, the
                // delete landed mid-linking and its deferred PHYSICAL phase is
                // ours — we are the one thread that knows this tower is now
                // quiescent, so we help the delete to completion (mark uppers,
                // unlink, retire). Otherwise the deleter read `height` and runs it
                // itself. Exactly one side observes the other; nobody waits.
                //
                // AcqRel: Release publishes the tower-link CASes above to the
                // physical-phase runner; Acquire (when we see DEL_PENDING) pairs
                // with the deleter's fetch_or so we observe its level-0 mark.
                let prev = (*new_node)
                    .linked_height
                    .swap(height as u8, Ordering::AcqRel);
                if prev & DEL_PENDING != 0 {
                    // The delete already linearized (mark + size decrement done);
                    // inherit its physical phase. `out_preds` are this insert's
                    // predecessors — possibly stale, which unlink_at_level /
                    // recover_pred tolerate (null entries fall back to HEAD).
                    self.physical_delete(new_node, out_preds);
                }

                // Update current_height hint if this node is taller
                let mut cur = self.current_height.load(Ordering::Relaxed);
                while height > cur {
                    match self.current_height.compare_exchange_weak(
                        cur,
                        height,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(h) => cur = h,
                    }
                }

                return (Ok(new_node), top);
            }
        }
    }

    /// Internal remove implementation.
    ///
    /// # Precondition
    ///
    /// The caller **must** hold a pinned epoch guard (`G::pin()`) for the
    /// duration of this call. Without it, concurrent reclamation may free
    /// nodes that this traversal is still reading, and the CAS-based mark
    /// sequence may operate on a freed node.
    pub(crate) fn remove_internal<Q>(
        &self,
        key: &Q,
        start_preds: Option<(&[SkipNodePtr<T>; MAX_LEVEL], usize)>,
    ) -> Option<*mut SkipNode<T>>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        loop {
            // Find the node using O(log n) traversal, with optional start preds
            // SAFETY: find_position with max_height=MAX_LEVEL writes preds[level] and
            // succs[level] for all levels 0..MAX_LEVEL-1. No uninitialized reads.
            let mut preds: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
            let mut succs: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
            self.find_position(key, MAX_LEVEL, start_preds, &mut preds, &mut succs);

            if succs[0].is_null() {
                return None;
            }

            // SAFETY: All `succs` and `preds` entries are guard-protected node pointers
            // set by `find_position`. `succs[0]` is non-null (checked above). We read
            // immutable fields (sentinel flag, key, height) and perform atomic CAS
            // operations on next pointers. The guard held by the caller prevents
            // reclamation of any traversed node during this operation.
            unsafe {
                // Defensive check: skip deallocated nodes
                if (*succs[0]).is_sentinel() {
                    return None;
                }
                if (*succs[0]).key().borrow() != key {
                    return None;
                }

                // Eligibility — the SAME resolution as every read (the one-predicate rule): a
                // DELETE-marked candidate is "absent" (answering `None` agrees
                // with every concurrent read); an UPDATE-marked one resolves to
                // its live same-key replacement, which is the node this delete
                // must target. A node whose tower is still being linked IS
                // eligible (reads report it present); tower safety is handled
                // inside `mark_and_unlink` by deferring the physical phase until
                // tower quiescence — delete never waits on the inserter, and
                // there is no reachability/publication gate left to spuriously
                // fail a present key.
                let node = SkipNode::resolve_replacement(succs[0])?;

                if self.mark_and_unlink(node, &preds) {
                    // Logical delete won (level-0 mark). Physical unlink + retirement
                    // are handled internally by the claim winner — the caller must
                    // NOT retire the returned node; it is only an identity/result.
                    return Some(node);
                }

                // Lost the level-0 mark race. To another DELETE the key is gone —
                // answer None (their delete linearized first). To an UPDATE the
                // key is STILL PRESENT, carried by the replacement: returning
                // None for a continuously-present key is a
                // linearizability violation — retry onto the replacement
                // (lock-free: the update we lost to completed its splice).
                let lost_to = MarkedPtr::new(SkipNode::get_next(node, 0));
                if lost_to.is_delete_marked() {
                    return None;
                }
                debug_assert!(lost_to.is_update_marked());
            }
        }
    }

    /// LOGICAL delete: win the level-0 DELETE mark (the linearization point and
    /// ownership CAS), then hand the PHYSICAL phase to exactly one runner via the
    /// `linked_height` handshake (see the protocol block at the top of the file).
    ///
    /// Returns `true` iff this call won the level-0 mark — the delete succeeded.
    /// Returns `false` if another thread marked level 0 first (their delete).
    ///
    /// Retirement is INTERNAL: `physical_delete` (run here when the tower is
    /// already quiescent, or by the node's inserter when the mark landed
    /// mid-linking) unlinks every level and retires the node through the guard.
    /// Callers must NOT `defer_destroy` the node — see the `SortedCollection`
    /// `delete`/`remove` overrides and the map paths.
    ///
    /// This never waits: upper levels are not touched here at all, so there is no
    /// dependency on the inserter's linking-loop progress (lock-freedom).
    ///
    /// # Safety
    /// - The caller must hold a pinned epoch guard.
    /// - `node` must be a valid, guard-protected, non-sentinel node pointer.
    /// - `preds` must be a predecessor array from a `find_position` for `node`'s
    ///   key (staleness is tolerated; entries above the traversed range may be
    ///   null — `recover_pred` falls back to HEAD).
    unsafe fn mark_and_unlink(
        &self,
        node: SkipNodePtr<T>,
        preds: &[SkipNodePtr<T>; MAX_LEVEL],
    ) -> bool {
        // SAFETY: per this function's contract, `node` and `preds` entries are valid,
        // guard-protected pointers. All dereferences read atomic next pointers and
        // immutable keys on live nodes; CAS operations are atomic.
        unsafe {
            // OWNERSHIP + LINEARIZATION: first thread to mark level 0 wins. Safe
            // at ANY point of the node's life past its level-0 publishing CAS:
            // level-0 slots are only ever CAS'd after publication (plain stores
            // touch only unlinked upper levels), so this mark cannot be erased.
            let we_own = loop {
                let next = SkipNode::get_next(node, 0);
                let next_marked = MarkedPtr::new(next);

                if next_marked.is_any_marked() {
                    // Already marked by another thread - they own it
                    break false;
                }

                let marked = next_marked.with_mark(true);
                if SkipNode::cas_next_weak(node, 0, next, marked.as_raw()).is_ok() {
                    break true;
                }
                // CAS failed (successor changed under us), retry
            };

            if !we_own {
                return false;
            }

            // NOTE: `size` is deliberately NOT maintained here (see insert_internal):
            // the mark winner may be a remover, an insert-helper finishing a claimed
            // tombstone, or a set-API delete — map layers count at their OWN
            // linearization points (MapEntry: the value claim; Pair: the mark-win
            // result), so counting here would double-count map removals.

            // HANDSHAKE: arbitrate the physical phase on `linked_height`'s
            // modification order. Reading `height` proves the inserter's linking
            // loop is done (tower quiescent) — we run the teardown. Reading less
            // means the inserter is still linking: its publication `swap` will
            // return our DEL_PENDING flag and IT runs the teardown. Either way
            // exactly one runner, and we return at the mark without waiting.
            //
            // AcqRel: Acquire (when we read `height`) pairs with the inserter's
            // swap-Release so the teardown sees every tower link; Release makes
            // our level-0 mark visible to an inserter that reads the flag.
            let prev = (*node)
                .linked_height
                .fetch_or(DEL_PENDING, Ordering::AcqRel);
            if prev == (*node).height() as u8 {
                self.physical_delete(node, preds);
            }

            true
        }
    }

    /// PHYSICAL delete phase: mark every upper level (top-down), unlink the node
    /// at every level, and retire it through the guard. Runs EXACTLY ONCE per
    /// deleted node — by the winner of the `linked_height` handshake: the deleter
    /// whose `fetch_or` read `height`, or the inserter whose publication `swap`
    /// read `DEL_PENDING` (a delete landed while it was still linking).
    ///
    /// # Safety
    /// - The caller must hold a pinned epoch guard.
    /// - `node` must be level-0 DELETE-marked (logical delete linearized) and its
    ///   tower QUIESCENT: the caller won the `linked_height` handshake, proving no
    ///   insert CAS or plain tower store for this node is or will be in flight.
    ///   That is what makes marking + unlinking every level UAF-free under real
    ///   reclamation — no late re-link of a retired node, no mark erased by
    ///   `set_next_relaxed`.
    /// - `preds` as in [`Self::mark_and_unlink`] (stale/null entries tolerated).
    unsafe fn physical_delete(&self, node: SkipNodePtr<T>, preds: &[SkipNodePtr<T>; MAX_LEVEL]) {
        // SAFETY: per contract — valid guard-protected pointers; atomic ops on
        // live nodes; quiescent tower makes the full sweep sound.
        unsafe {
            let height = (*node).height();
            let node_key = (*node).key();
            #[cfg(debug_assertions)]
            let mut unlink_reasons = [0u8; MAX_LEVEL];

            // Upper levels, top-down: mark (so traversals snip instead of route),
            // then unlink. Concurrent traversals may help unlink any level once
            // it is marked; unlink_at_level tolerates "already unlinked".
            for level in (1..height).rev() {
                loop {
                    let next = SkipNode::get_next(node, level);
                    let next_marked = MarkedPtr::new(next);

                    if next_marked.is_any_marked() {
                        // Already marked (traversal-side help) - skip
                        break;
                    }

                    let marked = next_marked.with_mark(true);
                    if SkipNode::cas_next_weak(node, level, next, marked.as_raw()).is_ok() {
                        break;
                    }
                    // CAS failed, retry
                }

                let mut pred = preds[level];
                let _r = self.unlink_at_level(level, &mut pred, node, node_key, None, preds);
                #[cfg(debug_assertions)]
                {
                    unlink_reasons[level] = _r;
                }
            }

            // Level 0 (already marked by the logical phase).
            let mut pred = preds[0];
            let _r0 = self.unlink_at_level(0, &mut pred, node, node_key, None, preds);
            #[cfg(debug_assertions)]
            {
                unlink_reasons[0] = _r0;
            }

            // RETIREMENT: the node is unlinked at every level and, with the tower
            // quiescent, can never be re-linked — safe for deferred reclamation.
            #[cfg(debug_assertions)]
            self.debug_assert_unreachable(node, &unlink_reasons);
            self.guard
                .defer_destroy(node, <SkipNode<T> as CollectionNode<T>>::dealloc_ptr);
        }
    }

    /// Debug canary: walk EVERY level end-to-end and abort if `node` is still
    /// reachable — retiring a reachable node is the use-after-free precursor.
    /// Debug-assertions builds only (run release repros with
    /// `RUSTFLAGS="-C debug-assertions=yes"`).
    #[cfg(debug_assertions)]
    unsafe fn debug_assert_unreachable(&self, node: SkipNodePtr<T>, reasons: &[u8; MAX_LEVEL]) {
        // SAFETY: called under the physical-phase runner's pin; we only read
        // atomic next pointers of guard-protected nodes.
        unsafe {
            for level in (0..(*node).height()).rev() {
                let mut prev: SkipNodePtr<T> = self.head();
                let mut curr = MarkedPtr::unmask(SkipNode::get_next(self.head(), level));
                let mut hops = 0usize;
                while !curr.is_null() && hops < 10_000_000 {
                    if std::ptr::eq(curr, node) {
                        // Count ALL in-edges to `node` at this level (walk the
                        // entire level once more): >1 ⇒ DOUBLE-LINK.
                        let mut in_edges = 0usize;
                        let mut scan = self.head();
                        let mut scan_hops = 0usize;
                        while !scan.is_null() && scan_hops < 10_000_000 {
                            let nxt = SkipNode::get_next(scan, level);
                            if std::ptr::eq(MarkedPtr::unmask(nxt), node) {
                                in_edges += 1;
                            }
                            scan = MarkedPtr::unmask(nxt);
                            if std::ptr::eq(scan, node) {
                                // continue THROUGH node to catch later edges
                            }
                            scan_hops += 1;
                        }
                        let holder_edge = MarkedPtr::new(SkipNode::get_next(prev, level));
                        let holder_l0 = MarkedPtr::new(SkipNode::get_next(prev, 0));
                        let key_rel = if std::ptr::eq(prev, self.head()) {
                            "HEAD"
                        } else if (*prev).key() < (*node).key() {
                            "<"
                        } else if (*prev).key() == (*node).key() {
                            "=="
                        } else {
                            ">"
                        };
                        panic!(
                            "RETIRE-WHILE-REACHABLE: node {:p} (height {}) still linked at \
                             level {} (own slot marked: {}, linked_height: {:#x}); HOLDER {:p}: \
                             key {} node.key, edge marked@level: {} (UPD: {}, DEL: {}), \
                             holder level-0 marked: {} (UPD: {}, DEL: {}), holder linked_height: \
                             {:#x}, holder height: {}, hops from head: {}, IN-EDGES at level: {}, \
                             unlink reason@level: {} (1=cas 2=null 3=sent 4=keygt 6=repl 7=enull 8=esent 9=ekeygt)",
                            node,
                            (*node).height(),
                            level,
                            MarkedPtr::new(SkipNode::get_next(node, level)).is_any_marked(),
                            (*node).linked_height.load(Ordering::Relaxed),
                            prev,
                            key_rel,
                            holder_edge.is_any_marked(),
                            holder_edge.is_update_marked(),
                            holder_edge.is_delete_marked(),
                            holder_l0.is_any_marked(),
                            holder_l0.is_update_marked(),
                            holder_l0.is_delete_marked(),
                            (*prev).linked_height.load(Ordering::Relaxed),
                            (*prev).height(),
                            hops,
                            in_edges,
                            reasons[level],
                        );
                    }
                    prev = curr;
                    curr = MarkedPtr::unmask(SkipNode::get_next(curr, level));
                    hops += 1;
                }
            }
        }
    }
}

impl<T: Ord, G: Guard> Default for SkipList<T, G> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, G: Guard> Drop for SkipList<T, G> {
    fn drop(&mut self) {
        // SAFETY: `drop` has exclusive `&mut self` access, so no concurrent operations
        // are possible. We walk the level-0 chain starting from HEAD, deallocating
        // each node. Every node was allocated by `alloc_with_key` and is visited
        // exactly once (level-0 chain covers all nodes). HEAD is embedded inline
        // and is not deallocated here.
        unsafe {
            let mut curr = SkipNode::get_next(self.head(), 0);
            curr = MarkedPtr::unmask(curr);

            while !curr.is_null() {
                let next = SkipNode::get_next(curr, 0);
                let next_marked = MarkedPtr::new(next);

                // INVARIANT CHECK: No marked nodes (DELETE or UPDATE) should remain
                // at drop time — every logically deleted/replaced node must have
                // been physically unlinked by its teardown (which always completes
                // within the deleter's, updater's, or inserter's own call) before
                // the collection can be dropped (&mut self ⇒ no in-flight ops).
                if next_marked.is_any_marked() && !(*curr).is_sentinel() {
                    panic!(
                        "INVARIANT VIOLATION: Found marked (deleted/replaced) node at drop \
                         time!\nMarked nodes should have been physically unlinked before drop."
                    );
                }

                let next_clean = next_marked.as_ptr();
                SkipNode::dealloc_node(curr);
                curr = next_clean;
            }

            // head_node is embedded in SkipList - no deallocation needed
        }
    }
}

// Safety: SkipList is thread-safe
unsafe impl<T: Send, G: Guard> Send for SkipList<T, G> {}
unsafe impl<T: Send + Sync, G: Guard> Sync for SkipList<T, G> {}

// ============================================================================
// SortedCollection trait implementation
// ============================================================================

impl<T: Ord, G: Guard> SortedCollectionInternal<T> for SkipList<T, G> {
    type Guard = G;
    type Node = SkipNode<T>;
    type NodePosition = SkipNodePosition<T>;

    fn guard(&self) -> &G {
        &self.guard
    }

    unsafe fn insert_from_internal(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition> {
        let start_preds = position.map(|pos| pos.preds());
        // SAFETY: find_position writes preds at all traversed levels (0..top-1).
        // valid_height tracks the boundary so reuse as start_preds is safe.
        let mut preds: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
        let (node_res, top) = self.insert_internal(key, start_preds, &mut preds);

        node_res
            .ok()
            .map(|node| SkipNodePosition::new(preds, node, top))
    }

    unsafe fn remove_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
    ) -> Option<Self::NodePosition> {
        let start_preds = position.map(|pos| pos.preds());
        self.remove_internal(key, start_preds)
            .map(|node| SkipNodePosition::from_node(node))
    }

    unsafe fn find_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
        is_match: bool,
    ) -> Option<Self::NodePosition> {
        let start_preds = position.map(|pos| pos.preds());
        // SAFETY: find_position with max_height=MAX_LEVEL writes all entries.
        let mut preds: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
        let mut succs: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
        let top = self.find_position(key, MAX_LEVEL, start_preds, &mut preds, &mut succs);

        if is_match {
            // Exact match required
            if !succs[0].is_null() {
                // SAFETY: `succs[0]` is non-null (just checked), set by `find_position`
                // during guard-protected traversal. Reading sentinel flag and key are
                // safe on a valid, non-reclaimed node.
                unsafe {
                    // Defensive check: skip deallocated nodes
                    if (*succs[0]).is_sentinel() {
                        return None;
                    }
                    // Exact match counts only if VISIBLE — the same predicate as
                    // contains/find_node_internal (`find -> Some` while
                    // `contains -> false` on the same thread is non-linearizable).
                    // An UPDATE-marked candidate resolves to its live same-key
                    // replacement; DELETE-marked resolves to absent.
                    if (*succs[0]).key().borrow() == key
                        && let Some(carrier) = SkipNode::resolve_replacement(succs[0])
                    {
                        return Some(SkipNodePosition::new(preds, carrier, top));
                    }
                }
            }
            None
        } else {
            // Return position at insertion point (preds). NOTE: `preds[0]` may be
            // the HEAD sentinel (key below all elements) — callers must not call
            // `NodePosition::key()` on a predecessor position (its `# Safety` forbids it).
            Some(SkipNodePosition::new(preds, preds[0], top))
        }
    }

    /// Apply a function to the given node's value.
    unsafe fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        let curr = MarkedPtr::unmask(node);

        if curr.is_null() || curr == self.head() {
            return None;
        }

        // SAFETY: `curr` is non-null and not HEAD (both checked above). The
        // caller holds a guard, so the node cannot be reclaimed. Reading
        // the immutable key field is safe on a valid, initialized node.
        unsafe { Some(f((*curr).key())) }
    }

    unsafe fn first_node_internal(&self) -> Option<*mut Self::Node> {
        // SAFETY: HEAD is always valid (embedded in SkipList). We traverse the
        // level-0 chain reading atomic next pointers. The guard (held by caller
        // or implicitly via SortedCollection::first()) protects against reclamation.
        // Yield only live carriers — the same resolution as contains/find/insert/
        // delete (the one-predicate rule): DELETE-marked nodes are skipped; an UPDATE-marked node
        // is skipped too, but its replacement (the very next node, same key) is
        // the natural next candidate, so a plain walk that yields the first
        // unmarked node implements "follow the replacement chain" for free.
        unsafe {
            // Start from head's next at level 0 (bottom level has all nodes)
            let mut curr = MarkedPtr::unmask(SkipNode::get_next(self.head(), 0));

            while !curr.is_null() {
                if SkipNode::is_visible(curr) {
                    return Some(curr);
                }
                curr = MarkedPtr::unmask(SkipNode::get_next(curr, 0));
            }

            None
        }
    }

    unsafe fn next_node_internal(&self, node: *mut Self::Node) -> Option<*mut Self::Node> {
        let node = MarkedPtr::unmask(node);

        if node.is_null() {
            return None;
        }

        // SAFETY: `node` is non-null (checked above) and guard-protected. We read
        // atomic next pointers at level 0, advancing to the next live carrier
        // (same resolution as contains/find — the one-predicate rule); DELETE-marked nodes are
        // skipped. SAME-KEY REPLACEMENT SKIP: the key at
        // `node` was already yielded; a forward-splice UPDATE links `node`'s
        // replacement (equal key) right after it, and update-chains can extend
        // this run — yielding any of them would produce the key twice. Keys are
        // unique among live carriers, so skip every candidate whose key equals
        // `node`'s; the genuine successor has a strictly greater key. All
        // traversed nodes are protected by the guard.
        unsafe {
            // HEAD / sentinel inputs carry no key (don't read it — the SortedList
            // analog of this skip read the key unconditionally and panics on the
            // HEAD sentinel — it carries no key): nothing was yielded, so nothing to skip.
            let node_key = if std::ptr::eq(node, self.head()) || (*node).is_sentinel() {
                None
            } else {
                Some((*node).key())
            };
            let mut curr = MarkedPtr::unmask(SkipNode::get_next(node, 0));

            while !curr.is_null() {
                let same_key = match node_key {
                    Some(k) if !(*curr).is_sentinel() => (*curr).key() == k,
                    _ => false,
                };
                if SkipNode::is_visible(curr) && !same_key {
                    return Some(curr);
                }
                curr = MarkedPtr::unmask(SkipNode::get_next(curr, 0));
            }
        }

        None
    }

    /// Insert a sentinel node with MAX_LEVEL height.
    ///
    /// For skip lists, sentinels need MAX_LEVEL height to enable O(log n)
    /// searches within buckets in split-ordered hash tables.
    ///
    unsafe fn insert_sentinel(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition> {
        let start_preds = position.map(|pos| pos.preds());
        let mut preds = [ptr::null_mut(); MAX_LEVEL];
        let (node_res, top) =
            self.insert_internal_with_height(key, MAX_LEVEL, start_preds, &mut preds);

        node_res
            .ok()
            .map(|node| SkipNodePosition::new(preds, node, top))
    }

    /// Forward-splice node-replacement update; see the dedicated protocol
    /// section comment below ("Node-replacement UPDATE (forward splice)").
    /// Retirement of the replaced node is INTERNAL (physical-phase winner), per
    /// the trait contract.
    ///
    /// NOTE: do not call this on a `SkipList<MapEntry<K, V>>` — mixing
    /// node-replacement with the value-CAS/tombstone protocol is outside the map
    /// contract (same rule as set-API mutation on a value-CAS map).
    unsafe fn update_internal(
        &self,
        position: Option<&Self::NodePosition>,
        new_value: T,
    ) -> Option<Self::NodePosition> {
        let start_preds = position.map(|pos| pos.preds());
        let mut value = new_value;
        loop {
            let mut preds: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
            let mut succs: [SkipNodePtr<T>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
            let top = self.find_position(&value, MAX_LEVEL, start_preds, &mut preds, &mut succs);

            let curr = succs[0];
            if curr.is_null() {
                return None;
            }

            // SAFETY: `curr` and all preds/succs entries are guard-protected
            // nodes from `find_position` (caller holds the pin per the trait
            // contract). `new_node` is exclusively owned until the splice CAS
            // publishes it; every non-published exit reclaims it via
            // `take_value_unlinked` + `dealloc_node_no_drop`.
            unsafe {
                if (*curr).is_sentinel() || (*curr).key() != &value {
                    return None;
                }

                // Match the replaced node's height: `preds` from this traversal
                // then covers every level the new tower needs.
                let height = (*curr).height();
                let new_node = SkipNode::alloc_with_key(value, height);

                // SPLICE — the linearization point on success.
                let spliced = loop {
                    let next = SkipNode::get_next(curr, 0);
                    let next_marked = MarkedPtr::new(next);
                    if next_marked.is_any_marked() {
                        break false; // a delete or another update marked first
                    }
                    // Unpublished: plain store; the CAS publishes with Release.
                    SkipNode::set_next_relaxed(new_node, 0, next);
                    let marked_new = MarkedPtr::new(new_node).with_update_mark(true);
                    if SkipNode::cas_next_weak(curr, 0, next, marked_new.as_raw()).is_ok() {
                        break true;
                    }
                };

                if !spliced {
                    value = (*new_node).take_value_unlinked();
                    SkipNode::dealloc_node_no_drop(new_node);
                    if MarkedPtr::new(SkipNode::get_next(curr, 0)).is_delete_marked() {
                        // Deleted under us: this update linearizes after that
                        // delete and fails (reads agree the key is absent).
                        return None;
                    }
                    // Replaced under us: retry against the live replacement
                    // (lock-free: the update we lost to completed its splice).
                    continue;
                }

                // `curr` is now a level-0-marked zombie — run or hand off its
                // physical teardown exactly as delete does. `size` is untouched
                // (replacement, not removal).
                let prev = (*curr)
                    .linked_height
                    .fetch_or(DEL_PENDING, Ordering::AcqRel);
                if prev == height as u8 {
                    self.physical_delete(curr, &preds);
                }

                // Link the replacement's tower. `preds` may be stale post-splice
                // (insert_at_level / recover_pred tolerate that, including the
                // equal-key run while `curr` is still linked at upper levels).
                let mut new_preds = preds;
                let node_key = (*new_node).key();
                for level in 1..height {
                    let mut pred = new_preds[level];
                    if !self.insert_at_level(level, &mut pred, new_node, node_key, &new_preds) {
                        break;
                    }
                    new_preds[level] = pred;
                }

                // Tower-quiescence publication for the NEW node — identical to
                // insert's: if a delete or another update marked it mid-link, we
                // inherit its deferred physical phase.
                let prev_new = (*new_node)
                    .linked_height
                    .swap(height as u8, Ordering::AcqRel);
                if prev_new & DEL_PENDING != 0 {
                    self.physical_delete(new_node, &new_preds);
                }

                return Some(SkipNodePosition::new(new_preds, new_node, top));
            }
        }
    }
}

impl<T: Ord, G: Guard> SortedCollection<T> for SkipList<T, G> {
    fn contains(&self, key: &T) -> bool {
        let _guard = G::pin();
        self.search_key_internal(key)
    }

    /// Overrides the trait default, which retires the removed node itself.
    /// SkipList retirement is INTERNAL — `physical_delete` runs exactly once
    /// (deleter or inserter, per the `linked_height` handshake) and is the only
    /// place a node is `defer_destroy`d. Retiring here too would double-free.
    fn delete(&self, key: &T) -> bool {
        let _guard = G::pin();
        // SAFETY: guard pinned above.
        unsafe { self.remove_from_internal(None, key) }.is_some()
    }

    /// Overrides the trait default for the same reason as [`Self::delete`]: the
    /// node must not be retired here. Cloning after the logical removal is safe —
    /// the pin keeps the node alive even if its (internal) retirement was already
    /// deferred.
    fn remove(&self, key: &T) -> Option<T>
    where
        T: Clone,
    {
        let _guard = G::pin();
        // SAFETY: guard pinned above.
        let pos = unsafe { self.remove_from_internal(None, key) }?;
        // SAFETY: node pointer from the removal above; guard still pinned.
        unsafe { self.apply_on_internal(pos.node_ptr(), |entry| entry.clone()) }
    }

    // `update` uses the default `SortedCollection::update` (a presence check): a set has
    // no value to update. The key->value maps use `SkipList<MapEntry<K,V>>` (value-CAS)
    // or `SkipList<Pair<K,V>>` (node-replacement via `update_internal` below).
}

// ============================================================================
// Node-replacement UPDATE (forward splice) — SortedCollectionInternal::update_internal
// ============================================================================
//
// `update_internal` replaces the key's carrier node with a fresh node holding
// the new value, with NO window where the key is absent and NO waiting on any
// other thread:
//
//   Level 0:  pred ──► curr ─────────────► succ
//                 splice CAS: curr.next[0]: succ → new|UPDATE_MARK
//   Level 0:  pred ──► curr ──U──► new ──► succ        ← LINEARIZATION POINT
//
// The single CAS atomically marks `curr` replaced AND publishes `new` (same
// key) right after it: every reader either lands on `curr` pre-CAS (live) or
// follows the UPDATE mark to `new` post-CAS (`resolve_replacement`) — the key
// is continuously present. `curr`'s teardown then reuses the DELETE machinery
// verbatim: it is a level-0-marked zombie, so the `linked_height` handshake
// picks exactly one runner for `physical_delete` (this updater if the tower is
// quiescent, else `curr`'s still-linking inserter), which unlinks every level —
// the level-0 unlink splices `pred -> new` automatically because the
// replacement IS `unmask(curr.next[0])` — and retires `curr`. Traversal snips
// of the UPDATE-marked level-0 slot preserve the splice for the same reason.
//
// Mark interactions on next[0] (each is one CAS expecting an UNMARKED value,
// so the illegal UPD|DEL double-mark state is never created):
//   - update vs delete: whoever marks first wins. A losing update observes
//     DELETE and reports the key absent (linearizes after the delete); a losing
//     delete observes UPDATE and retries onto the replacement.
//   - update vs update: the loser follows the winner's replacement and retries.
//   - update vs `curr`'s in-flight insert: the splice is a level-0 CAS (always
//     safe post-publication); teardown defers to the inserter via the handshake.
//
// `size` is untouched throughout: a replacement is not a membership change.

// ============================================================================
// Value-CAS map: SkipList<MapEntry<K, V>>
// ============================================================================
//
// `SkipList<Pair<K, V>>` is intentionally NOT a map: SkipList's node-replacement
// UPDATE was removed because its lock-free *helping* variant could not guarantee
// the old node was unreachable before retirement (use-after-free under real
// reclamation, reproduced with EpochGuard + ASan). Non-helping node-replacement
// remains in SortedList/SkipTrie and is validated epoch-safe there; SkipList uses
// the `MapEntry<K, V>` payload instead (value behind an `AtomicPtr`).
//
// The node is NEVER replaced, and the value pointer carries BOTH the value and
// the entry's logical presence (null = tombstone), giving single-atomic
// update↔remove linearization (see `map_entry.rs`):
//   - `update`  = value-pointer CAS that fails on the tombstone; retires the old
//     value through the guard (epoch-safe by construction).
//   - `remove`  = CLAIM the value (swap null) — the linearization point — then
//     clone-under-pin, retire the claimed value, and physically delete the node.
//   - reads (`get`/`get_ref`/`find_and_apply`/`contains`) treat a null value
//     pointer as absent.
//   - `insert` HELPS finish a claimed (tombstoned) duplicate's physical delete
//     (mark + unlink via the shared machinery) and retries — it never waits.
// Set-API mutation (`SortedCollection::insert/delete`) on a map-typed list is NOT
// part of the map contract: it bypasses the tombstone protocol (a set-delete can
// still race a value-CAS update) — use the `MapCollection` API exclusively.

impl<K, V, G: Guard> MapCollectionInternal<K, V> for SkipList<MapEntry<K, V>, G>
where
    K: Eq + Ord,
{
    type Guard = G;
    type Node = SkipNode<MapEntry<K, V>>;

    fn guard(&self) -> &Self::Guard {
        &self.guard
    }

    fn insert_internal(&self, key: K, value: V) -> Option<*mut Self::Node> {
        let mut entry = MapEntry::new(key, value);
        loop {
            let mut preds: [SkipNodePtr<MapEntry<K, V>>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
            // Map-layer counting: +1 at the successful publish. The shared set path
            // does NOT count (a shared counter there serializes disjoint inserts).
            match SkipList::insert_internal(self, entry, None, &mut preds).0 {
                Ok(node) => {
                    self.size.fetch_add(1, Ordering::Relaxed);
                    return Some(node);
                }
                Err(e) => {
                    // A node with this key is physically present and UNMARKED at
                    // level 0 (set-level Err contract). If its value is LIVE, the
                    // insert genuinely fails — consistent with map reads, which
                    // report a live entry present even while its tower is still
                    // being linked. If it is a claimed TOMBSTONE, the remove
                    // already linearized (reads say "absent"), so failing would be
                    // a program-order linearizability violation; instead HELP the
                    // remover's physical phase to completion (mark level 0 via the
                    // shared machinery — never wait for the remover) and retry.
                    // Lock-free: every retry follows our own successful mark/snip
                    // or another thread's completed operation.
                    entry = e;
                    let mut hpreds: [SkipNodePtr<MapEntry<K, V>>; MAX_LEVEL] =
                        [ptr::null_mut(); MAX_LEVEL];
                    let mut succs: [SkipNodePtr<MapEntry<K, V>>; MAX_LEVEL] =
                        [ptr::null_mut(); MAX_LEVEL];
                    self.find_position(entry.key(), MAX_LEVEL, None, &mut hpreds, &mut succs);
                    let existing = succs[0];
                    if existing.is_null() {
                        continue; // blocker already gone — retry
                    }
                    // SAFETY: `existing` is guard-protected (caller's pin), set by
                    // find_position during the traversal above.
                    unsafe {
                        if (*existing).is_sentinel()
                            || CollectionNode::key(&*existing).key() != entry.key()
                            || !SkipNode::is_visible(existing)
                        {
                            // Different/absent/already-marked blocker: the
                            // re-traversal on retry handles it (snips marked).
                            continue;
                        }
                        if !CollectionNode::key(&*existing).value_ptr().is_null() {
                            return None; // genuine live duplicate (drops the new entry)
                        }
                        // Claimed tombstone: finish the delete ourselves. Win or
                        // lose the mark race (vs the remover or another helper),
                        // the node is marked afterwards and the retry's traversal
                        // snips it. Retirement is internal to the claim winner.
                        self.mark_and_unlink(existing, &hpreds);
                    }
                }
            }
        }
    }

    /// Claim-first map removal (see [`MapEntry::claim_value`]): the CLAIM is the
    /// linearization point. This pointer-returning trait method cannot hand the
    /// claimed value to the caller, so it retires it unread — prefer
    /// [`MapCollection::remove`] (overridden below), which returns the claimed value.
    /// Returns `Some(node)` iff this call won the claim (the removal). The node is
    /// NEVER the caller's to retire — SkipList retirement is internal to the
    /// physical-phase winner (see `mark_and_unlink`); the default
    /// [`MapCollection::remove`] (which retires) must stay overridden.
    fn remove_internal(&self, key: &K) -> Option<*mut Self::Node> {
        // SAFETY: caller holds a pinned guard (trait contract).
        unsafe {
            let node = self.find_node_internal(key)?;
            let entry = CollectionNode::key(&*node);
            let claimed = entry.claim_value()?;
            // Map-layer counting: -1 at the claim (the unique-winner linearization
            // point). The shared mark/unlink machinery does NOT count — its mark
            // winner may be an insert-helper, which must not decrement.
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.guard
                .defer_destroy(claimed, MapEntry::<K, V>::drop_value);
            self.finish_remove_claimed(node, key);
            Some(node)
        }
    }

    fn find_internal(&self, key: &K) -> Option<*mut Self::Node> {
        let node = self.find_node_internal(key)?;
        // Tombstone gate: a claimed entry is logically ABSENT even while its node is
        // still linked and unmarked (map remove claims first, then marks + unlinks).
        // SAFETY: `node` is guard-protected (caller's pin).
        if unsafe { CollectionNode::key(&*node).value_ptr() }.is_null() {
            return None;
        }
        Some(node)
    }

    fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
        let curr = MarkedPtr::unmask(node);
        if curr.is_null() || std::ptr::eq(curr, self.head()) {
            return None;
        }
        // SAFETY: `curr` is non-null and not HEAD; the caller holds a guard, so the
        // node — and the value observed by the load below — cannot be reclaimed for
        // the duration of `f`. The null check guards the tombstone state (entry
        // claimed by a concurrent remove between our find and this load).
        unsafe {
            let entry = CollectionNode::key(&*curr);
            let value = entry.value_ptr();
            if value.is_null() {
                return None; // linearizes after the concurrent remove's claim
            }
            Some(f(entry.key(), &*value))
        }
    }

    /// Epoch-safe value update: locate the entry by key, CAS in the new value, and
    /// retire the OLD VALUE via the guard. The node is never replaced or retired —
    /// only the old value. Returns `true` if the key existed, `false` otherwise.
    ///
    /// Linearizes at the value CAS ([`MapEntry::replace_value`]), which FAILS on a
    /// remover's tombstone — so an update can never succeed after a remove it races,
    /// and a successful update's value is always observable (the racing remover
    /// returns it). This is what makes update↔remove histories linearizable.
    fn update_internal(&self, key: K, value: V) -> bool {
        // find_node_internal precondition (pinned guard) is met by the caller.
        let node = match self.find_node_internal(&key) {
            Some(n) => n,
            None => return false,
        };
        let curr = MarkedPtr::unmask(node);
        if curr.is_null() || std::ptr::eq(curr, self.head()) {
            return false;
        }
        // SAFETY: `curr` is a live, guard-protected entry node; `old` (if returned) is
        // unreachable once the CAS lands (the entry now points at the new value), so
        // retiring it through the epoch guard is correct.
        unsafe {
            let entry = CollectionNode::key(&*curr);
            match entry.replace_value(value) {
                Some(old) => {
                    self.guard.defer_destroy(old, MapEntry::<K, V>::drop_value);
                    true
                }
                // Tombstone: a concurrent remove claimed the entry — the update
                // linearizes after it and fails.
                None => false,
            }
        }
    }

    fn len_internal(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }
}

impl<K, V, G: Guard> MapCollection<K, V> for SkipList<MapEntry<K, V>, G>
where
    K: Eq + Ord,
{
    /// Claim-first removal. Linearizes at [`MapEntry::claim_value`] — the unique
    /// winner of the value claim IS the removal, regardless of who later wins the
    /// physical level-0 mark. This serializes remove against the value-CAS `update`
    /// on the SAME atomic (the value pointer), so `remove` always returns the
    /// then-current value: a history like `update(k,v2)->true` + `remove(k)->v1` is
    /// impossible (the update either precedes the claim — remove returns v2 — or
    /// observes the tombstone and fails).
    fn remove(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let _guard = Self::Guard::pin();
        // SAFETY: guard pinned above for the find, claim, clone, and physical delete.
        unsafe {
            let node = self.find_node_internal(key)?;
            let entry = CollectionNode::key(&*node);
            // REMOVE linearization point: claim the value (null tombstone).
            let claimed = entry.claim_value()?;
            // Map-layer counting: -1 at the claim (unique winner; see remove_internal).
            self.size.fetch_sub(1, Ordering::Relaxed);
            // Clone under OUR pin, then retire: a concurrent pinned reader may still
            // hold a borrow of the claimed value — it must never be freed inline.
            let value = (*claimed).clone();
            self.guard
                .defer_destroy(claimed, MapEntry::<K, V>::drop_value);
            // Physical deletion (mark + unlink + retire node) is cleanup after the
            // linearization point above; retirement happens inside the
            // physical-phase winner (`mark_and_unlink` handshake), never here.
            self.finish_remove_claimed(node, key);
            Some(value)
        }
    }
}

/// Zero-copy guarded value read for the value-CAS map.
impl<K, V, G: Guard> SkipList<MapEntry<K, V>, G>
where
    K: Eq + Ord,
{
    /// Physically delete a CLAIMED entry node: drive the shared logical/physical
    /// delete machinery (`mark_and_unlink`). Called after [`MapEntry::claim_value`]
    /// won (the logical removal already happened). Retirement of both the value
    /// (caller, before this) and the node (physical-phase winner, inside) is
    /// handled elsewhere — this returns nothing and the caller must NOT retire.
    ///
    /// Losing the level-0 mark race here is fine: a map-insert helper (see
    /// `insert_internal`) or a set-API delete may finish the physical phase first.
    ///
    /// # Safety
    /// - The caller must hold a pinned epoch guard covering `node` since before the
    ///   claim.
    /// - `node` must be the entry node whose value this thread just claimed.
    unsafe fn finish_remove_claimed(&self, node: *mut SkipNode<MapEntry<K, V>>, key: &K) {
        // Re-locate to collect fresh predecessors. Identity check: keys are unique
        // among unmarked nodes, so as long as `node` is unmarked it is reachable at
        // level 0 and find_position must land exactly on it. If it doesn't, another
        // thread won the level-0 mark (only marked nodes become unreachable) — the
        // physical phase is theirs.
        let mut preds: [SkipNodePtr<MapEntry<K, V>>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
        let mut succs: [SkipNodePtr<MapEntry<K, V>>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
        self.find_position(key, MAX_LEVEL, None, &mut preds, &mut succs);

        if succs[0] != node {
            // SAFETY (debug only): `node` is guard-protected per contract.
            debug_assert!(
                MarkedPtr::new(unsafe { SkipNode::get_next(node, 0) }).is_any_marked(),
                "claimed node unreachable at level 0 but not marked"
            );
            return;
        }

        // SAFETY: contract of this fn — `node` valid, guard-protected; `preds`
        // fresh from find_position for `node`'s key.
        unsafe {
            self.mark_and_unlink(node, &preds);
        }
    }

    /// Look up `key` and return a guard-protected handle to its value — the map analog
    /// of [`SortedCollection::find`]. The returned `GuardedRef` holds its own pinned
    /// epoch guard, so the value stays alive — and a concurrent value-CAS `update`
    /// cannot reclaim it — for as long as the handle is held, without cloning `V`.
    ///
    /// Drop the handle promptly: while alive it pins the epoch and stalls reclamation.
    /// To keep a value past the read, deref and clone the inner `V`; for an owned copy
    /// directly, use [`MapCollection::get`](crate::data_structures::hash::MapCollection::get).
    pub fn get_ref(&self, key: &K) -> Option<<G as Guard>::GuardedRef<'_, V>> {
        let _guard = G::pin();
        // SAFETY: guard pinned above for the find + value-pointer load.
        let node = self.find_node_internal(key)?;
        let curr = MarkedPtr::unmask(node);
        if curr.is_null() || std::ptr::eq(curr, self.head()) {
            return None;
        }
        // SAFETY: under the pin, `curr` and its current value are protected. `make_ref`
        // pins its OWN guard before referencing the value pointer, so the returned handle
        // keeps the value alive after `_guard` drops; a concurrent `update`/`remove`'s
        // CAS/claim + deferred free of the old value cannot run while any such handle
        // is held. The null check guards the tombstone state (entry claimed by a
        // concurrent remove between our find and this load) — logically absent.
        unsafe {
            let value_ptr = CollectionNode::key(&*curr).value_ptr();
            if value_ptr.is_null() {
                return None;
            }
            Some(G::make_ref(value_ptr))
        }
    }
}

// ============================================================================
// Node-replacement map: SkipList<Pair<K, V>>
// ============================================================================
//
// The second map backend: the value lives INLINE in the node — no `AtomicPtr`
// indirection, so reads touch one allocation (better locality than the
// `MapEntry` value-CAS backend) — and `update` is the forward-splice node
// replacement (`update_internal` above): the key is never absent during an
// update, and no operation waits on any other thread.
//
// update↔remove still serialize on ONE atomic — the carrier's level-0 next
// pointer (update's splice CAS vs delete's DELETE-mark CAS, both expecting an
// unmarked value) — so `remove` always returns the then-current value: an
// update either precedes the mark (the remove targets and returns the
// REPLACEMENT's value, via the lost-to-update retry) or observes the mark and fails.
// This is the node-replacement analog of the value-CAS map's then-current-value guarantee.
//
// `Pair`'s Eq/Ord are KEY-ONLY, so lookups and replacement matching ignore the
// value. Retirement is internal to the physical-phase winner — the default
// `MapCollection::remove` (which retires) is overridden, and `update_internal`
// does NOT `defer_destroy` the old node, UNLIKE SortedList/SkipTrie.

impl<K, V, G: Guard> MapCollectionInternal<K, V> for SkipList<Pair<K, V>, G>
where
    K: Eq + Ord,
{
    type Guard = G;
    type Node = SkipNode<Pair<K, V>>;

    fn guard(&self) -> &Self::Guard {
        &self.guard
    }

    fn insert_internal(&self, key: K, value: V) -> Option<*mut Self::Node> {
        let mut preds: [SkipNodePtr<Pair<K, V>>; MAX_LEVEL] = [ptr::null_mut(); MAX_LEVEL];
        // Err = a live carrier of this key exists (genuine duplicate, consistent
        // with map reads). Marked blockers are snipped/resolved inside the
        // set-level retry — this backend has no tombstones, so no helping loop
        // is needed here (unlike the MapEntry impl).
        // Map-layer counting: +1 at the successful publish (the shared set path
        // does not count). Node-replacement update is net-zero by construction.
        let node = SkipList::insert_internal(self, Pair::new(key, value), None, &mut preds)
            .0
            .ok()?;
        self.size.fetch_add(1, Ordering::Relaxed);
        Some(node)
    }

    fn remove_internal(&self, key: &K) -> Option<*mut Self::Node> {
        // Logical removal (won the level-0 mark; retries past lost-to-update —
        // lost-to-update retry). Retirement is INTERNAL: the caller must NOT retire the
        // returned node (the default `MapCollection::remove` stays overridden).
        // Map-layer counting: -1 iff THIS call won the mark (unique winner).
        let node = SkipList::remove_internal(self, key, None)?;
        self.size.fetch_sub(1, Ordering::Relaxed);
        Some(node)
    }

    fn find_internal(&self, key: &K) -> Option<*mut Self::Node> {
        // resolve_replacement inside makes an UPDATE-marked carrier resolve to
        // its replacement — reads never miss a continuously-present key.
        self.find_node_internal(key)
    }

    fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
        let curr = MarkedPtr::unmask(node);
        if curr.is_null() || std::ptr::eq(curr, self.head()) {
            return None;
        }
        // SAFETY: non-null and not HEAD (checked); the caller holds a pinned
        // guard (trait contract), so the node cannot be reclaimed while `f`
        // runs. The pair is immutable for the node's lifetime — updates REPLACE
        // nodes, never mutate them.
        unsafe {
            let node_ref = &*curr;
            if node_ref.is_sentinel() {
                return None;
            }
            let pair = CollectionNode::key(node_ref);
            Some(f(&pair.key, &pair.value))
        }
    }

    fn update_internal(&self, key: K, value: V) -> bool {
        // SAFETY: guard pinned by caller (MapCollection::update). Old-node
        // retirement is INTERNAL to the node-replacement update (uniform trait
        // contract) — never defer here.
        unsafe {
            SortedCollectionInternal::update_internal(self, None, Pair::new(key, value)).is_some()
        }
    }

    fn len_internal(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }
}

impl<K, V, G: Guard> MapCollection<K, V> for SkipList<Pair<K, V>, G>
where
    K: Eq + Ord,
{
    /// Overrides the trait default, which retires the removed node — SkipList
    /// retirement is internal to the physical-phase winner; retiring here too
    /// would double-free. Cloning under the pin is safe even though internal
    /// retirement may already be deferred.
    ///
    /// Linearizes at the level-0 DELETE mark, serialized against update's
    /// splice CAS on the same atomic — the returned value is therefore always
    /// the then-current one (see the section comment above).
    fn remove(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let _guard = G::pin();
        // SAFETY: guard pinned above; the node returned by `remove_internal` is
        // guard-protected for the duration of the clone.
        unsafe {
            let node = MapCollectionInternal::remove_internal(self, key)?;
            Some(CollectionNode::key(&*node).value.clone())
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::{SkipList, SkipNodePosition};
    use crate::DeferredGuard;
    use crate::data_structures::{SortedCollection, SortedCollectionInternal};

    // Positions must be move-only to prevent stale pointer reuse across guard epochs.
    static_assertions::assert_not_impl_any!(SkipNodePosition<i32>: Copy, Clone);

    // Common tests (test_basic_operations, test_concurrent_operations, etc.)
    // are in sorted_collection_core_tests.rs and run via deferred_collection_tests.rs

    #[test]
    fn test_trait_methods() {
        // Tests SortedCollection trait methods directly on SkipList
        let list: SkipList<i32, DeferredGuard> = SkipList::new();

        // SAFETY: test uses DeferredGuard — all frees deferred to collection drop; single-threaded.
        unsafe {
            // insert_from_internal
            assert!(list.insert_from_internal(5, None).is_some());
            assert!(list.insert_from_internal(3, None).is_some());
            assert!(list.insert_from_internal(5, None).is_none()); // Duplicate

            // find_from_internal
            assert!(list.find_from_internal(None, &5, true).is_some());
            assert!(list.find_from_internal(None, &99, true).is_none());

            // remove_from_internal — retirement is INTERNAL (the physical-phase
            // winner defer_destroys the node); the returned position is identity
            // only and must NOT be manually deallocated.
            let deleted_node = list.remove_from_internal(None, &5);
            assert!(deleted_node.is_some());
            assert!(list.find_from_internal(None, &5, true).is_none());
        }
    }

    #[test]
    fn test_basic_insert_delete() {
        // Basic test: insert values and delete some
        let list: SkipList<i32, DeferredGuard> = SkipList::new();

        // Insert some nodes
        for i in 0..20 {
            list.insert(i);
        }

        // Verify all values exist
        for i in 0..20 {
            assert!(list.contains(&i), "Value {} should exist", i);
        }

        // Delete some values. Node retirement is INTERNAL (deferred through the
        // collection's guard by the physical-phase winner) — no manual cleanup.
        for i in (0..20).step_by(2) {
            // SAFETY: test uses DeferredGuard — all frees deferred to collection drop; single-threaded.
            let deleted = unsafe { list.remove_from_internal(None, &i) };
            assert!(deleted.is_some(), "Should delete {}", i);
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
        // Tests that contains/find work correctly after deleting nearby nodes.
        // Internally, search_key_internal and find_position must recover when they
        // encounter marked (deleted) nodes during traversal.
        let list: SkipList<i32, DeferredGuard> = SkipList::new();

        // Insert nodes
        for i in 0..100 {
            list.insert(i);
        }

        // SAFETY: test uses DeferredGuard — all frees deferred to collection drop; single-threaded.
        unsafe {
            // Delete node 50
            let deleted_node = list.remove_from_internal(None, &50);
            assert!(deleted_node.is_some());

            // Verify neighbors are still findable (traversal must skip marked node 50)
            assert!(list.contains(&49));
            assert!(!list.contains(&50));
            assert!(list.contains(&51));
            assert!(list.contains(&60));

            // Find with position hint from a node before the deleted one
            let pos_40 = list.find_from_internal(None, &40, true).unwrap();
            let pos_60 = list.find_from_internal(Some(&pos_40), &60, true);
            assert!(pos_60.is_some());

            // Deleted node retirement is INTERNAL — no manual cleanup.
        }
    }

    #[test]
    fn test_physical_delete_unlinks_zombie_behind_equal_key() {
        // Regression for the deferred-physical-delete protocol: a deleted zombie Z
        // can coexist at ROUTING levels with a re-inserted node N of the SAME key,
        // with Z linked BEHIND N (pred -> N(k) -> Z(k) at level 1). The unlink walk
        // used to conclude "already unlinked" at the first key >= k — satisfied by
        // N while Z was still reachable — so physical_delete retired a reachable
        // node (use-after-free under real reclamation). The walk must advance
        // THROUGH equal keys and stop only at a strictly greater key.
        use super::{MAX_LEVEL, SkipNode};
        use crate::data_structures::MarkedPtr;

        let list: SkipList<i32, DeferredGuard> = SkipList::new();
        // SAFETY: single-threaded test; DeferredGuard defers all frees to drop.
        unsafe {
            // Z: key 10, height 2, fully linked + published.
            let mut preds: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            let z = list
                .insert_internal_with_height(10, 2, None, &mut preds)
                .0
                .expect("insert Z");

            // Simulate a delete whose physical phase is still pending: DELETE-mark
            // Z at level 0 only (the logical delete), touch nothing else — the
            // state a deleter leaves when it defers teardown via the handshake.
            let next0 = SkipNode::get_next(z, 0);
            let m = MarkedPtr::new(next0);
            assert!(!m.is_any_marked());
            SkipNode::cas_next(z, 0, next0, m.with_mark(true).as_raw()).expect("mark Z@0");

            // Re-insert key 10, same height: the traversal snips Z at level 0 and
            // links N at level 1 in FRONT of Z (Z is unmarked there) — the
            // equal-key coexistence window.
            let mut preds2: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            let n = list
                .insert_internal_with_height(10, 2, None, &mut preds2)
                .0
                .expect("re-insert N over the marked zombie");
            assert!(!std::ptr::eq(n, z));
            // Confirm the window: Z still linked at level 1 behind N.
            assert!(
                std::ptr::eq(MarkedPtr::unmask(SkipNode::get_next(n, 1)), z),
                "precondition: zombie linked behind equal-key node at level 1"
            );

            // Run Z's deferred physical phase, as the handshake winner would.
            let mut p: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            let mut s: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            list.find_position(&10, MAX_LEVEL, None, &mut p, &mut s);
            list.physical_delete(z, &p);

            // Z must be physically unreachable at EVERY level before retirement is
            // sound; N must remain the (only) member.
            for level in 0..2 {
                let mut curr = MarkedPtr::unmask(SkipNode::get_next(list.head(), level));
                while !curr.is_null() {
                    assert!(
                        !std::ptr::eq(curr, z),
                        "zombie still reachable at level {level} after physical_delete"
                    );
                    curr = MarkedPtr::unmask(SkipNode::get_next(curr, level));
                }
            }
            assert!(
                list.contains(&10),
                "re-inserted key must survive the teardown"
            );
        }
    }

    #[test]
    fn test_unlink_advance_snips_marked_equal_key_candidate() {
        // Regression for a SOLO infinite loop (caught by gdb in update's
        // physical_delete): `unlink_at_level`'s advance stepped onto a candidate
        // whose own slot was marked. The loop-top marked-slot check then bounced
        // to `recover_pred` (stale preds -> HEAD) and re-advanced onto the same
        // candidate — a closed cycle that no other thread was left to break.
        // The advance must HELP-SNIP a marked candidate instead of stepping onto
        // it (same rule as `insert_at_level`'s advance).
        //
        // State under test (level 0): HEAD -> Z(10, marked, STILL LINKED) -> X(10,
        // marked) — Z is an equal-key zombie in the walk's path to X. Run
        // `unlink_at_level(0, node=X)` on a watchdog thread: it must terminate
        // (pre-fix it spins forever) and X must be unlinked.
        use super::{MAX_LEVEL, SkipNode};
        use crate::data_structures::MarkedPtr;

        let list: &'static SkipList<i32, DeferredGuard> = Box::leak(Box::new(SkipList::new()));
        // SAFETY: single-threaded construction; DeferredGuard defers frees; the
        // list is intentionally leaked so the watchdog can fail the test cleanly
        // (leaking the spinning worker) instead of hanging the suite on a regression.
        let (head_addr, x_addr) = unsafe {
            // X: key 10, height 1, inserted normally. HEAD -> X.
            let mut preds: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            let x = list
                .insert_internal_with_height(10, 1, None, &mut preds)
                .0
                .expect("insert X");

            // Z: same key, hand-spliced IN FRONT of X (HEAD -> Z -> X), then
            // UPDATE-marked pointing at X — the shape an update's spliced-out old
            // node has (old.next = replacement|UPDATE) while still linked.
            let z = SkipNode::alloc_with_key(10, 1);
            SkipNode::set_next_relaxed(z, 0, x); // unpublished: plain store
            SkipNode::cas_next(list.head(), 0, x, z).expect("splice Z before X");
            let zn = SkipNode::get_next(z, 0);
            SkipNode::cas_next(z, 0, zn, MarkedPtr::new(zn).with_update_mark(true).as_raw())
                .expect("mark Z@0");

            // Logically mark X@0 (the unlink contract: node is already marked).
            let xn = SkipNode::get_next(x, 0);
            SkipNode::cas_next(x, 0, xn, MarkedPtr::new(xn).with_mark(true).as_raw())
                .expect("mark X@0");

            (list.head() as usize, x as usize)
        };

        // Watchdog: the call must terminate. On regression the worker spins
        // forever — we leak it and FAIL via the timeout instead of hanging.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let head = head_addr as *mut SkipNode<i32>;
            let x = x_addr as *mut SkipNode<i32>;
            // SAFETY: pointers built above on the leaked ('static) list; X is
            // level-0 marked per the function's contract.
            let reason = unsafe {
                let mut pred = head;
                let mut preds: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
                preds[1] = head; // recover_pred fallback target — closes the pre-fix cycle
                list.unlink_at_level(0, &mut pred, x, &10, None, &preds)
            };
            let _ = tx.send(reason);
        });
        let reason = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("unlink_at_level did not terminate: solo advance/recover cycle on a marked equal-key candidate");

        // X must be physically unreachable at level 0 (Z was help-snipped en route).
        unsafe {
            let x = x_addr as *mut SkipNode<i32>;
            let mut curr = MarkedPtr::unmask(SkipNode::get_next(list.head(), 0));
            while !curr.is_null() {
                assert!(
                    !std::ptr::eq(curr, x),
                    "X still reachable at level 0 (unlink reason {reason})"
                );
                curr = MarkedPtr::unmask(SkipNode::get_next(curr, 0));
            }
        }
    }

    #[test]
    fn test_unlink_ignores_dead_pred_frozen_slot() {
        // Regression for the release-mode SIGSEGV (retire-while-reachable, unlink
        // reason 4): `unlink_at_level` drew "already unlinked" conclusions from a
        // DEAD predecessor's FROZEN slot. A pred that was torn down at this level
        // keeps a marked snapshot of its old successor — which can sort STRICTLY
        // GREATER than the victim while the victim is still linked elsewhere.
        // The walk must recover off marked slots BEFORE any conclusion.
        //
        // Construction: P_dead(10) marked at level 1 with frozen slot -> 30;
        // victim V(20) linked at level 1 between HEAD and 30; physical_delete(V)
        // is handed preds[1] = P_dead. Old code: reads frozen 30 (> 20), returns
        // "already unlinked", retires V still linked at level 1. New code:
        // recovers and properly unlinks V.
        use super::{MAX_LEVEL, SkipNode};
        use crate::data_structures::MarkedPtr;

        let list: SkipList<i32, DeferredGuard> = SkipList::new();
        // SAFETY: single-threaded test; DeferredGuard defers all frees to drop.
        unsafe {
            let mut p: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            let p_dead = list
                .insert_internal_with_height(10, 2, None, &mut p)
                .0
                .expect("insert 10");
            let n30 = list
                .insert_internal_with_height(30, 2, None, &mut p)
                .0
                .expect("insert 30");
            assert!(std::ptr::eq(
                MarkedPtr::unmask(SkipNode::get_next(p_dead, 1)),
                n30
            ));

            // Freeze P_dead at level 1 pointing at 30 (simulates a teardown that
            // marked level 1 but whose unlink hasn't been helped yet).
            let frozen = SkipNode::get_next(p_dead, 1);
            SkipNode::cas_next(
                p_dead,
                1,
                frozen,
                MarkedPtr::new(frozen).with_mark(true).as_raw(),
            )
            .expect("freeze P_dead@1");

            // Insert the victim: its level-1 traversal snips P_dead@1 and links
            // V between HEAD and 30.
            let victim = list
                .insert_internal_with_height(20, 2, None, &mut p)
                .0
                .expect("insert victim");
            assert!(std::ptr::eq(
                MarkedPtr::unmask(SkipNode::get_next(victim, 1)),
                n30
            ));

            // Delete the victim through the shared machinery, but with FORGED
            // stale preds: preds[1] = P_dead (its frozen slot points at 30 > 20).
            let mut preds: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            preds[0] = list.head();
            preds[1] = p_dead;
            assert!(
                list.mark_and_unlink(victim, &preds),
                "delete must win the mark"
            );

            // The victim must be GONE from level 1 (the old code left it linked —
            // the debug canary aborts there; this assert catches it in any build).
            let mut curr = MarkedPtr::unmask(SkipNode::get_next(list.head(), 1));
            while !curr.is_null() {
                assert!(
                    !std::ptr::eq(curr, victim),
                    "victim still linked at level 1 after physical delete"
                );
                curr = MarkedPtr::unmask(SkipNode::get_next(curr, 1));
            }
            assert!(!list.contains(&20));
            assert!(list.contains(&10) && list.contains(&30));
        }
    }

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

        let list: Arc<SkipList<i32, DeferredGuard>> = Arc::new(SkipList::new());

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

        // Thread 2: Delete some of the values being inserted.
        // Must go through `delete()`, which retires the node via the collection's
        // DeferredGuard (freed only at collection drop): deallocating immediately
        // here would free a node the inserter thread may still be traversing — a
        // real use-after-free (DeferredGuard's pin is a no-op and protects nothing).
        let deleter = thread::spawn(move || {
            for i in 100..200 {
                // Try to delete - might succeed if insert happened first
                list2.delete(&i);
            }
        });

        inserter.join().unwrap();
        deleter.join().unwrap();

        // Verify list is consistent - all remaining values should be findable
        let mut count = 0;
        // SAFETY: test uses DeferredGuard — all frees deferred to collection drop; single-threaded.
        unsafe {
            let mut node = list.first_node_internal();
            while let Some(n) = node {
                count += 1;
                node = list.next_node_internal(n);
            }
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
        use crate::guard::DeferredGuard;
        use std::sync::Arc;
        use std::thread;

        let list: Arc<SkipList<i32, DeferredGuard>> = Arc::new(SkipList::default());
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
        let values = list.to_vec();
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
    fn test_map_remove_returns_latest_update() {
        // Lost-update regression: remove and update serialize on the value pointer, so
        // remove must return the THEN-CURRENT value, including a just-installed
        // update, and an update racing a remove must fail (never silently vanish).
        use crate::data_structures::MapEntry;
        use crate::data_structures::hash::MapCollection;
        let map: SkipList<MapEntry<i32, u64>, DeferredGuard> = SkipList::new();

        assert!(MapCollection::insert(&map, 1, 10));
        assert!(MapCollection::update(&map, 1, 11));
        assert_eq!(MapCollection::remove(&map, &1), Some(11));
        assert_eq!(MapCollection::remove(&map, &1), None); // claim already gone
        assert!(!MapCollection::contains(&map, &1));
        // Key fully removed physically too: a fresh insert must succeed.
        assert!(MapCollection::insert(&map, 1, 12));
        assert_eq!(MapCollection::get(&map, &1), Some(12));
    }

    #[test]
    fn test_map_tombstone_window_reads_absent() {
        // Deterministically construct the claim→mark window of a map remove (entry
        // claimed, node still linked and unmarked) and verify EVERY map path treats
        // the entry as logically absent — this is the state a concurrent remover
        // exposes between its linearization point and its physical cleanup.
        use crate::data_structures::MapEntry;
        use crate::data_structures::hash::map_collection::MapCollection;
        use crate::guard::Guard;
        let map: SkipList<MapEntry<i32, u64>, DeferredGuard> = SkipList::new();
        assert!(MapCollection::insert(&map, 2, 20));

        DeferredGuard::pin(); // no-op for DeferredGuard; kept to mirror the protocol
        let node = map.find_node_internal(&2).expect("key present");
        // SAFETY: single-threaded test, DeferredGuard defers frees to drop.
        let entry = unsafe { crate::data_structures::CollectionNode::key(&*node) };
        let claimed = entry.claim_value().expect("first claim wins");

        // Claimed-but-unmarked: absent on all map read/write paths.
        assert!(!MapCollection::contains(&map, &2));
        assert_eq!(MapCollection::get(&map, &2), None);
        assert!(map.get_ref(&2).is_none());
        assert!(
            !MapCollection::update(&map, 2, 99),
            "update must fail on a tombstone (linearizes after the remove)"
        );
        assert_eq!(
            MapCollection::remove(&map, &2),
            None,
            "second remove must lose the claim"
        );

        // Finish the physical removal exactly as the remover would (node
        // retirement is internal to the physical-phase winner), then verify the
        // key slot is reusable.
        // SAFETY: we claimed the value above (unique winner); node observed live
        // under the pin.
        unsafe {
            MapEntry::<i32, u64>::drop_value(claimed);
            map.finish_remove_claimed(node, &2);
        }
        assert!(MapCollection::insert(&map, 2, 22));
        assert_eq!(MapCollection::get(&map, &2), Some(22));
    }

    #[test]
    fn test_map_insert_helps_finish_tombstone() {
        // An insert that hits a physically-present but CLAIMED (tombstoned)
        // duplicate must HELP finish the remover's physical phase and succeed
        // WITHOUT waiting for the remover — never report "exists" while reads
        // report "absent", and never spin on another thread's progress. The
        // remover deliberately parks inside its claim→cleanup window for the
        // whole insert; the insert must complete anyway (lock-freedom).
        use crate::data_structures::MapEntry;
        use crate::data_structures::hash::map_collection::MapCollection;
        use crate::guard::Guard;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrd};
        use std::thread;

        let map: Arc<SkipList<MapEntry<i32, u64>, DeferredGuard>> = Arc::new(SkipList::new());
        assert!(MapCollection::insert(&*map, 7, 70));

        // Deterministic handshake (no sleep-based racing): the main thread inserts
        // only AFTER the claim happened, so it provably enters the tombstone
        // window; the remover resumes its cleanup only AFTER the insert finished,
        // so the insert provably got no help from the remover.
        let claimed_flag = Arc::new(AtomicBool::new(false));
        let inserted_flag = Arc::new(AtomicBool::new(false));
        let m2 = Arc::clone(&map);
        let flag2 = Arc::clone(&claimed_flag);
        let ins2 = Arc::clone(&inserted_flag);
        let remover = thread::spawn(move || {
            DeferredGuard::pin(); // no-op for DeferredGuard; kept to mirror the protocol
            let node = m2.find_node_internal(&7).expect("key present");
            // SAFETY: DeferredGuard defers frees to collection drop.
            let entry = unsafe { crate::data_structures::CollectionNode::key(&*node) };
            let claimed = entry.claim_value().expect("claim wins");
            flag2.store(true, AtomicOrd::Release);
            // Stay parked in the claim→cleanup window until the insert completed
            // on its own. (Test-only handshake spin — not part of the protocol.)
            while !ins2.load(AtomicOrd::Acquire) {
                thread::yield_now();
            }
            // SAFETY: unique claimer; node observed live under the pin. The late
            // cleanup must be harmless: the helper already finished the physical
            // phase (finish_remove_claimed detects the lost mark and backs off).
            unsafe {
                MapEntry::<i32, u64>::drop_value(claimed);
                m2.finish_remove_claimed(node, &7);
            }
        });

        // Wait for the claim, then insert the same key: the physical duplicate is
        // a tombstone, so the insert helps mark + unlink it and succeeds while the
        // remover is still parked mid-removal.
        while !claimed_flag.load(AtomicOrd::Acquire) {
            std::hint::spin_loop();
        }
        assert!(MapCollection::insert(&*map, 7, 71));
        assert_eq!(MapCollection::get(&*map, &7), Some(71));
        inserted_flag.store(true, AtomicOrd::Release);
        remover.join().unwrap();
        // The remover's late cleanup must not have clobbered the new entry.
        assert_eq!(MapCollection::get(&*map, &7), Some(71));
    }

    #[test]
    fn test_update_window_key_never_absent() {
        // Deterministically construct the UPDATE splice window: old node Z is
        // UPDATE-marked with replacement N spliced after it, but Z's physical
        // teardown has NOT run (the updater is "paused" mid-operation). Every
        // path must treat the key as continuously present, carried by N.
        use super::{DEL_PENDING, MAX_LEVEL, SkipNode};
        use crate::data_structures::MarkedPtr;
        use std::sync::atomic::Ordering as AtomOrd;

        let list: SkipList<i32, DeferredGuard> = SkipList::new();
        list.insert(5);
        list.insert(20);
        // SAFETY: single-threaded test; DeferredGuard defers frees to drop.
        unsafe {
            let mut preds: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            let z = list
                .insert_internal_with_height(10, 2, None, &mut preds)
                .0
                .expect("insert Z");

            // Manual splice: N (same key) linked after Z, Z UPDATE-marked.
            let n = SkipNode::alloc_with_key(10, 2);
            let succ = SkipNode::get_next(z, 0);
            SkipNode::set_next_relaxed(n, 0, succ);
            SkipNode::cas_next(
                z,
                0,
                succ,
                MarkedPtr::new(n).with_update_mark(true).as_raw(),
            )
            .expect("splice");
            // Publish N's tower-quiescence (height 2, level 1 left unlinked —
            // legal: towers are routing only).
            (*n).linked_height.store(2, AtomOrd::Release);

            // Window assertions: the key is present, carried by N.
            assert!(list.contains(&10), "key must never be absent mid-update");
            assert!(
                std::ptr::eq(list.find_node_internal(&10).expect("present"), n),
                "reads must resolve to the replacement"
            );
            assert!(
                !list.insert(10),
                "insert must see the replacement as a duplicate"
            );

            // Iteration yields the key exactly once — from Z (as if yielded
            // pre-splice) the same-key replacement is skipped (duplicate-free iteration).
            use crate::data_structures::CollectionNode;
            let first = self::SortedCollectionInternal::first_node_internal(&list).unwrap();
            assert_eq!(*CollectionNode::key(&*first), 5);
            let second = self::SortedCollectionInternal::next_node_internal(&list, first).unwrap();
            assert!(std::ptr::eq(second, n), "iteration lands on the carrier");
            let third = self::SortedCollectionInternal::next_node_internal(&list, second).unwrap();
            assert_eq!(
                *CollectionNode::key(&*third),
                20,
                "no duplicate 10 after the carrier"
            );
            let after_z = self::SortedCollectionInternal::next_node_internal(&list, z).unwrap();
            assert_eq!(
                *CollectionNode::key(&*after_z),
                20,
                "advancing from the replaced node must skip its same-key replacement"
            );

            // Finish the paused updater's teardown of Z, exactly as it would.
            let prev = (*z).linked_height.fetch_or(DEL_PENDING, AtomOrd::AcqRel);
            assert_eq!(prev, 2, "Z was published — teardown belongs to the updater");
            let mut p: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            let mut s: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            list.find_position(&10, MAX_LEVEL, None, &mut p, &mut s);
            list.physical_delete(z, &p);

            assert!(list.contains(&10));
            // Lost-to-update shape: delete after the update must succeed (on N).
            assert!(list.delete(&10), "delete must target the live carrier");
            assert!(!list.contains(&10));
        }
    }

    #[test]
    fn test_delete_loses_to_update_retries_onto_replacement() {
        // A delete that loses the level-0 race to an UPDATE must retry
        // onto the replacement and succeed — never report "absent" for a
        // continuously-present key. Single-threaded deterministic shape: the
        // window is open (Z UPD-marked, teardown pending) when delete runs.
        use super::{DEL_PENDING, MAX_LEVEL, SkipNode};
        use crate::data_structures::MarkedPtr;
        use std::sync::atomic::Ordering as AtomOrd;

        let list: SkipList<i32, DeferredGuard> = SkipList::new();
        // SAFETY: single-threaded test; DeferredGuard defers frees to drop.
        unsafe {
            let mut preds: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            let z = list
                .insert_internal_with_height(7, 1, None, &mut preds)
                .0
                .expect("insert Z");
            let n = SkipNode::alloc_with_key(7, 1);
            let succ = SkipNode::get_next(z, 0);
            SkipNode::set_next_relaxed(n, 0, succ);
            SkipNode::cas_next(
                z,
                0,
                succ,
                MarkedPtr::new(n).with_update_mark(true).as_raw(),
            )
            .expect("splice");
            (*n).linked_height.store(1, AtomOrd::Release);

            // The delete arrives mid-window: it must resolve Z -> N and delete N.
            assert!(
                list.delete(&7),
                "delete of a continuously-present key must succeed"
            );
            assert!(!list.contains(&7));

            // Late teardown of Z by the "paused" updater stays harmless.
            let prev = (*z).linked_height.fetch_or(DEL_PENDING, AtomOrd::AcqRel);
            assert_eq!(prev, 1);
            let mut p: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            let mut s: [*mut SkipNode<i32>; MAX_LEVEL] = [std::ptr::null_mut(); MAX_LEVEL];
            list.find_position(&7, MAX_LEVEL, None, &mut p, &mut s);
            list.physical_delete(z, &p);
            assert!(!list.contains(&7));
            assert!(list.insert(7), "slot reusable after both teardowns");
        }
    }

    #[test]
    fn test_pair_map_node_replacement_crud() {
        // The node-replacement map backend: update splices a replacement node
        // (key never absent), remove returns the then-current value (update and
        // analog — update and remove serialize on the level-0 next pointer).
        use crate::data_structures::Pair;
        use crate::data_structures::hash::MapCollection;
        let map: SkipList<Pair<i32, u64>, DeferredGuard> = SkipList::new();

        assert!(MapCollection::insert(&map, 1, 10));
        assert!(
            !MapCollection::insert(&map, 1, 999),
            "duplicate insert fails"
        );
        assert_eq!(MapCollection::get(&map, &1), Some(10));
        assert!(MapCollection::update(&map, 1, 11));
        assert_eq!(MapCollection::get(&map, &1), Some(11));
        assert!(MapCollection::update(&map, 1, 12));
        assert_eq!(
            MapCollection::remove(&map, &1),
            Some(12),
            "remove returns latest value"
        );
        assert_eq!(MapCollection::remove(&map, &1), None);
        assert!(
            !MapCollection::update(&map, 1, 13),
            "update of absent key fails"
        );
        assert!(MapCollection::insert(&map, 1, 14), "slot reusable");
        assert_eq!(MapCollection::get(&map, &1), Some(14));
        assert_eq!(MapCollection::len(&map), 1);
    }

    #[test]
    fn test_contains_after_sentinel_insert() {
        // Regression test: insert_sentinel creates a MAX_LEVEL-height node,
        // which raises current_height to MAX_LEVEL (32). search_key_internal must not
        // panic from an out-of-bounds preds[top] write.
        let list: SkipList<i32, DeferredGuard> = SkipList::new();

        // Insert a sentinel — this sets current_height to MAX_LEVEL
        // SAFETY: test uses DeferredGuard — all frees deferred to collection drop; single-threaded.
        let pos = unsafe { list.insert_sentinel(0, None) };
        assert!(pos.is_some(), "Sentinel insert should succeed");

        // Insert a regular node after the sentinel
        list.insert(42);

        // These calls would panic before the fix (preds[32] OOB)
        assert!(list.contains(&42));
        assert!(!list.contains(&99));
    }
}
