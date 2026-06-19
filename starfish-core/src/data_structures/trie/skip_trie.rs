//! Skip trie: a concurrent sorted map combining x-fast trie prefix
//! hashing with skip list buckets.
//!
//! Provides `SkipTrie<T, G>`, the `KeyBits` trait for order-preserving
//! bit encoding required by the trie's binary prefix search, and
//! `MapCollection<K, V>` for `SkipTrie<Pair<K,V>, G>` key-value usage.

use std::alloc::{Layout, alloc, dealloc};
use std::borrow::Borrow;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use crate::data_structures::hash::{MapCollection, MapCollectionInternal, MapNode};
use crate::data_structures::internal::{MarkedPtr, UnpublishedNode, load_consume};
use crate::data_structures::pair::Pair;
use crate::data_structures::{
    CollectionNode, NodePosition, SortedCollection, SortedCollectionInternal,
};
use crate::guard::Guard;

/// Order-preserving bit encoding for x-fast trie prefix computation.
///
/// The conversion must satisfy: `a < b ⟹ a.to_bits() < b.to_bits()`.
/// This is required for the x-fast trie's binary search on prefix length
/// to correctly locate predecessor nodes in key order.
pub trait KeyBits: Ord + Eq {
    /// Number of meaningful bits in the type's representation.
    const BIT_WIDTH: usize;
    /// Convert to an order-preserving u64 bit representation.
    fn to_bits(&self) -> u64;
}

impl KeyBits for i32 {
    const BIT_WIDTH: usize = 32;
    #[inline]
    fn to_bits(&self) -> u64 {
        // Flip sign bit: maps [-2^31, 2^31-1] to [0, 2^32-1]
        (*self as u32 ^ 0x8000_0000) as u64
    }
}

impl KeyBits for i64 {
    const BIT_WIDTH: usize = 64;
    #[inline]
    fn to_bits(&self) -> u64 {
        // Flip sign bit: maps [-2^63, 2^63-1] to [0, 2^64-1]
        *self as u64 ^ 0x8000_0000_0000_0000
    }
}

impl KeyBits for u32 {
    const BIT_WIDTH: usize = 32;
    #[inline]
    fn to_bits(&self) -> u64 {
        *self as u64
    }
}

impl KeyBits for u64 {
    const BIT_WIDTH: usize = 64;
    #[inline]
    fn to_bits(&self) -> u64 {
        *self
    }
}

impl KeyBits for usize {
    const BIT_WIDTH: usize = usize::BITS as usize;
    #[inline]
    fn to_bits(&self) -> u64 {
        *self as u64
    }
}

impl<K: KeyBits, V> KeyBits for Pair<K, V> {
    const BIT_WIDTH: usize = K::BIT_WIDTH;
    #[inline]
    fn to_bits(&self) -> u64 {
        self.key.to_bits()
    }
}

// =============================================================================
// SKIP TRIE — ARCHITECTURE & INVARIANTS
// =============================================================================
//
// A concurrent sorted set based on the SkipTrie data structure
// (Oshman & Shavit, PODC 2013). Provides amortized expected O(log log u)
// predecessor queries, where u is the key universe size.
//
// COMPONENTS:
//
//   ┌─────────────────────────────────────────────────────────────────────┐
//   │  SkipTrie                                                         │
//   │                                                                   │
//   │  ┌───────────────────────────────────────────────────────────┐     │
//   │  │  X-Fast Trie (hash table of prefixes)                    │     │
//   │  │  Stores all bit-prefixes of top-level skiplist nodes.    │     │
//   │  │  Binary search on prefix length → O(log log u) lookup.  │     │
//   │  └────────────────────────────┬──────────────────────────────┘     │
//   │                               │ pointers into top level           │
//   │                               ▼                                   │
//   │  ┌───────────────────────────────────────────────────────────┐     │
//   │  │  Truncated Skip List (height ≤ log log u)                │     │
//   │  │  Standard lock-free skip list, but with reduced height.  │     │
//   │  │  Uses Harris-style marked pointers for deletion/update.  │     │
//   │  └───────────────────────────────────────────────────────────┘     │
//   │                                                                   │
//   └─────────────────────────────────────────────────────────────────────┘
//
// =============================================================================
// TRUNCATED SKIP LIST (height = log log u)
// =============================================================================
//
// A standard skip list has O(log n) height. The truncated skip list caps
// its height at log(log(u)) levels. On its own, this means traversals
// that start from HEAD take O(n^ε) per level instead of O(1) expected.
// The x-fast trie compensates by providing O(log log u) starting positions
// directly into the top level of the skip list, so each search covers at
// most O(log log u) nodes per level × O(log log u) levels = O(log log u).
//
// Example (u = 2^30, log log u ≈ 5):
//
//   Level 4 (top): HEAD ──────────► 15 ─────────────────────────► 90 ──► NULL
//                    │                │                              │
//   Level 3:       HEAD ──────────► 15 ──────────► 50 ────────────► 90 ──► NULL
//                    │                │              │                │
//   Level 2:       HEAD ──► 5 ─────► 15 ──────────► 50 ──► 70 ────► 90 ──► NULL
//                    │       │        │              │       │        │
//   Level 1:       HEAD ──► 5 ──► 10 ► 15 ──► 30 ──► 50 ──► 70 ──► 90 ──► NULL
//                    │       │    │     │      │      │       │       │
//   Level 0:       HEAD ──► 5 ──► 10 ► 15 ► 20 ► 30 ► 50 ► 60 ► 70 ► 90 ► NULL
//
//   Nodes reaching the top level: 15, 90
//   These top-level nodes are registered in the x-fast trie.
//   find_lowest_ancestor returns a nearby top-level node as a hint,
//   and the skiplist search starts from there instead of HEAD.
//
// =============================================================================
// X-FAST TRIE (prefix hash table)
// =============================================================================
//
// The x-fast trie stores all bit-prefixes of the key bit values of top-level
// skiplist nodes (order-preserving via KeyBits trait). Each prefix entry
// contains two pointers:
//
//   pointers[0] = largest node in the 0-subtree (left child subtree)
//   pointers[1] = smallest node in the 1-subtree (right child subtree)
//
// Given a key universe of u values requiring B = ⌈log₂(u)⌉ bits, the trie
// has B+1 levels (0..=B). Each top-level node with key bits "b₀b₁...b_{B-1}"
// contributes B+1 prefix entries:
//
//   "" (root), "b₀", "b₀b₁", ..., "b₀b₁...b_{B-1}" (leaf)
//
// Binary tree interpretation (B = 4 bits, keys hashed to 4-bit values):
//
//                           ""  (root, level 0)
//                          /  \
//                        "0"  "1"  (level 1)
//                       / \    / \
//                    "00" "01" "10" "11"  (level 2)
//                    / \  / \  / \  / \
//                  ...  ...  ...  ...     (level 3)
//                  / \  / \  / \  / \
//                 leaves (level 4 = B)    ← actual skiplist nodes
//
// FIND LOWEST ANCESTOR (Algorithm 3 from paper):
// ─────────────────────────────────────────────────────────────────────────
// Binary search on prefix length to find the deepest existing prefix
// for the query key. At each step, the prefix's pointers[direction]
// gives a nearby top-level skiplist node. Total: O(log B) = O(log log u).
//
//   1. Start with range [0, B], midpoint = B/2
//   2. Check if prefix of length mid exists in hash table
//   3. If yes: use its pointer, search deeper (start = mid)
//   4. If no: search shallower (size /= 2)
//   5. Return best skiplist node found
//
// PREDECESSOR QUERY (full algorithm):
// ─────────────────────────────────────────────────────────────────────────
//
//   Query: predecessor(key=42)
//
//   Step 1: find_lowest_ancestor(42) → nearest top-level skiplist node
//           (O(log log u) via binary search on prefix length)
//
//   Step 2: Use the returned node as a hint to find_position_from,
//           which starts traversal from that node instead of HEAD
//           (O(log log u) levels × O(log log u) nodes per level)
//
//   Total: O(log log u) amortized expected
//
// The x-fast trie uses order-preserving key bits (via KeyBits trait) for
// prefix computation, and find_lowest_ancestor is wired into all search
// paths. The skip list height is truncated to log(log(u)) levels, with
// the trie providing O(log log u) starting positions into the skiplist.
//
// =============================================================================
// LOCK-FREE OPERATIONS (same protocols as SkipList)
// =============================================================================
//
// The TruncatedSkipList uses the same Harris-style lock-free algorithms
// as the SkipList (see skip_list.rs for detailed diagrams):
//
// INSERT: Bottom-up linking with CAS at each level.
//         If the node reaches the top level, it is also inserted into
//         the x-fast trie (all its prefixes).
//
// DELETE: Top-down marking (levels height..1), then mark level 0
//         (linearization point / ownership CAS), then physical unlink.
//         If the node was at the top level, it is removed from the
//         x-fast trie after unlinking.
//
// UPDATE (mark-then-insert, forward insertion):
// ─────────────────────────────────────────────────────────────────────────
//   Step 1: Mark + unlink higher levels of curr (top-down)
//   Step 2+3: CAS curr.next[0]: succ → new_node|UPDATE_MARK
//             (atomically marks curr and splices in new_node)
//   Step 4: Link new_node at higher levels (bottom-up)
//   Step 5: Unlink curr at level 0
//
//   Before:  ... ► B ────────► curr ──────► succ ► ...
//   After:   ... ► B ────────► new_node ──► succ ► ...
//                               curr (orphaned, deferred free)
//
//   The key is NEVER absent. Iterators following UPDATE_MARK on curr
//   are directed to new_node. The x-fast trie is updated: insert
//   new_node first, then remove curr (if either was a top-level node).
//
// RECOVERY STRATEGY: Same as SkipList — when a predecessor is marked,
//   use preds[level+1] from the saved array instead of restarting from HEAD.
//   O(1) amortized recovery.
//
// =============================================================================

// =============================================================================
// Constants and helpers
// =============================================================================

/// Maximum key universe size (2^30).
const MAX_U: usize = 1 << 30;

/// Default skiplist height. The paper uses log(log(u)) ≈ 4-5, which requires
/// Fallback skiplist height used for MAX_TRIE_LEVELS stack array sizing.
/// Actual height is log_log_u(key_universe_size), set at construction time.
const DEFAULT_SKIPLIST_HEIGHT: usize = 20;

/// Maximum number of levels in the truncated skiplist (0..=max_height).
/// Sized for stack arrays; actual height is typically much smaller (4-5).
const MAX_TRIE_LEVELS: usize = DEFAULT_SKIPLIST_HEIGHT + 1;

type SkipTrieNodePtr<T> = *mut SkipTrieNode<T>;

/// Predecessors array with valid height for position hint threading.
type SkipTriePreds<T> = ([SkipTrieNodePtr<T>; MAX_TRIE_LEVELS], usize);

/// Calculate log log u for skiplist height (paper's truncated height).
/// For u = 2^30: log(30) ≈ 4. For u = 2^64: log(64) = 6.
fn log_log_u(u: usize) -> usize {
    if u <= 2 {
        return 1;
    }
    let log_u = (usize::BITS - u.leading_zeros() - 1) as usize;
    std::cmp::max(1, (usize::BITS - log_u.leading_zeros() - 1) as usize)
}

// =============================================================================
// SkipTrieNode
// =============================================================================

/// Node in the truncated skiplist component of the SkipTrie.
///
/// Uses the flexible array member (FAM) pattern for a single heap allocation
/// per node: the struct fields and the next-pointer array are laid out
/// contiguously in memory. This halves allocator pressure compared to a
/// separate `Box<[AtomicPtr]>` and improves cache locality.
///
/// Logical deletion is encoded via pointer marking on the `next` pointers
/// (using MarkedPtr LSB flags), not via a separate boolean field. A node is
/// logically deleted when its `next[0]` pointer carries the DELETE mark.
///
/// `height_flags` layout: bits 0-6 = height (max 127), bit 7 = sentinel flag.
#[repr(C)]
pub struct SkipTrieNode<T> {
    key: MaybeUninit<T>,
    height_flags: u8,
    // Flexible array: pointers are allocated inline after this struct.
    // Layout: [next[0], next[1], ..., next[height]]
    // Total: height + 1 pointers (levels 0..=height)
    pointers: [AtomicPtr<SkipTrieNode<T>>; 0],
}

const SKIP_TRIE_SENTINEL_FLAG: u8 = 0x80;

impl<T> SkipTrieNode<T> {
    /// Calculate layout for a node with given height.
    /// Allocates height + 1 pointers (levels 0..=height).
    fn get_layout(height: usize) -> Layout {
        Layout::new::<Self>()
            .extend(Layout::array::<AtomicPtr<Self>>(height + 1).unwrap())
            .unwrap()
            .0
            .pad_to_align()
    }

    /// Allocate and initialize a new data node with a key.
    fn alloc_with_key(key: T, height: usize) -> *mut Self {
        debug_assert!(
            height <= 127,
            "height {height} would corrupt sentinel flag bit"
        );
        // SAFETY: Layout is computed from the node struct + flexible array of
        // `height + 1` AtomicPtrs. `alloc` returns a valid, aligned, non-null
        // pointer (or we abort via `handle_alloc_error`). We field-initialize
        // via `addr_of_mut!` + `write` to avoid creating an intermediate
        // reference to uninitialized memory, and zero-initialize the pointer
        // array to null (valid for AtomicPtr).
        unsafe {
            let layout = Self::get_layout(height);
            let ptr = alloc(layout) as *mut Self;
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }

            ptr::addr_of_mut!((*ptr).key).write(MaybeUninit::new(key));
            ptr::addr_of_mut!((*ptr).height_flags).write(height as u8);

            // Zero-initialize all height+1 pointers to null
            ptr::addr_of_mut!((*ptr).pointers)
                .cast::<AtomicPtr<Self>>()
                .write_bytes(0, height + 1);

            ptr
        }
    }

    /// Allocate and initialize a sentinel node.
    fn alloc_sentinel(height: usize) -> *mut Self
    where
        T: Default,
    {
        debug_assert!(
            height <= 127,
            "height {height} would corrupt sentinel flag bit"
        );
        // SAFETY: Same as `alloc_with_key`. Sentinel key is a valid
        // `T::default()` wrapped in MaybeUninit, so the key is always
        // initialized and can be dropped normally.
        unsafe {
            let layout = Self::get_layout(height);
            let ptr = alloc(layout) as *mut Self;
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }

            ptr::addr_of_mut!((*ptr).key).write(MaybeUninit::new(T::default()));
            ptr::addr_of_mut!((*ptr).height_flags).write(height as u8 | SKIP_TRIE_SENTINEL_FLAG);

            ptr::addr_of_mut!((*ptr).pointers)
                .cast::<AtomicPtr<Self>>()
                .write_bytes(0, height + 1);

            ptr
        }
    }

    /// Deallocate a node, dropping the key value.
    ///
    /// # Safety
    /// - The pointer must have been allocated by `alloc_with_key` or `alloc_sentinel`.
    /// - Must only be called once per node.
    unsafe fn dealloc_node(ptr: *mut Self) {
        // SAFETY: Caller guarantees `ptr` was allocated by `alloc_with_key` or
        // `alloc_sentinel`. We reconstruct the same Layout from the stored height,
        // drop the key (always valid — sentinels use T::default()), then dealloc
        // with the matching layout.
        unsafe {
            let height = ((*ptr).height_flags & !SKIP_TRIE_SENTINEL_FLAG) as usize;
            let layout = Self::get_layout(height);

            // Key is always initialized (data: alloc_with_key, sentinel: T::default())
            ptr::addr_of_mut!((*ptr).key).cast::<T>().drop_in_place();

            dealloc(ptr as *mut u8, layout);
        }
    }

    // =========================================================================
    // Accessor methods
    // =========================================================================

    #[inline(always)]
    fn key(&self) -> &T {
        // SAFETY: key is always initialized — data nodes via `alloc_with_key`,
        // sentinels via `alloc_sentinel` with `T::default()`.
        unsafe { self.key.assume_init_ref() }
    }

    #[inline(always)]
    fn is_sentinel(&self) -> bool {
        self.height_flags & SKIP_TRIE_SENTINEL_FLAG != 0
    }

    #[inline(always)]
    fn height(&self) -> usize {
        (self.height_flags & !SKIP_TRIE_SENTINEL_FLAG) as usize
    }

    // =========================================================================
    // Pointer access (FAM)
    // =========================================================================

    /// Get reference to the AtomicPtr at given index in the flexible array.
    #[inline(always)]
    unsafe fn pointer_at(&self, index: usize) -> &AtomicPtr<SkipTrieNode<T>> {
        debug_assert!(
            index <= self.height(),
            "level {index} > height {}",
            self.height()
        );
        // SAFETY: Callers guarantee `index <= height` (validated at call sites;
        // debug_assert catches violations in debug builds). The flexible array
        // was allocated with `height + 1` AtomicPtrs and initialized in
        // `alloc_with_key`/`alloc_sentinel`, so `pointers.as_ptr().add(index)`
        // is within bounds and properly aligned.
        unsafe { &*self.pointers.as_ptr().add(index) }
    }

    // =========================================================================
    // Next pointer accessors (mirrors SkipNode's interface)
    // =========================================================================

    /// Check if this node is logically deleted (level-0 next pointer is DELETE-marked).
    #[inline]
    fn is_logically_deleted(&self) -> bool {
        // SAFETY: Level 0 is always valid (all nodes have height >= 0).
        let next0 = unsafe { self.pointer_at(0) }.load(Ordering::Acquire);
        MarkedPtr::new(next0).is_delete_marked()
    }

    /// Load next pointer at level (consume ordering).
    /// Uses load_consume: Relaxed + compiler_fence on ARM (plain `ldr`),
    /// Acquire on x86 (plain `mov` under TSO). Safe for pointer-chasing
    /// traversals where data-dependency ordering suffices.
    #[inline(always)]
    fn get_next(&self, level: usize) -> *mut SkipTrieNode<T> {
        // SAFETY: Callers guarantee `level <= height`.
        unsafe { load_consume(self.pointer_at(level)) }
    }

    /// Store next pointer at level (Relaxed ordering).
    /// Used for initial setup of unpublished nodes before CAS-linking.
    #[inline(always)]
    fn set_next_relaxed(&self, level: usize, ptr: *mut SkipTrieNode<T>) {
        // SAFETY: Callers guarantee `level <= height`.
        unsafe { self.pointer_at(level) }.store(ptr, Ordering::Relaxed)
    }

    /// Weak CAS next pointer at level (Release/Acquire ordering).
    #[inline(always)]
    fn cas_next_weak(
        &self,
        level: usize,
        expected: *mut SkipTrieNode<T>,
        new: *mut SkipTrieNode<T>,
    ) -> Result<*mut SkipTrieNode<T>, *mut SkipTrieNode<T>> {
        // SAFETY: Callers guarantee `level <= height`.
        // Acquire on failure is required because `unlink_at_level` may
        // dereference the returned pointer immediately after a failed CAS.
        // (unsound on weakly-ordered architectures without it).
        unsafe { self.pointer_at(level) }.compare_exchange_weak(
            expected,
            new,
            Ordering::Release,
            Ordering::Acquire,
        )
    }

    /// Strong CAS next pointer at level (Release/Acquire ordering).
    #[inline(always)]
    fn cas_next(
        &self,
        level: usize,
        expected: *mut SkipTrieNode<T>,
        new: *mut SkipTrieNode<T>,
    ) -> Result<*mut SkipTrieNode<T>, *mut SkipTrieNode<T>> {
        // SAFETY: Callers guarantee `level <= height`.
        // Same rationale as `cas_next_weak`: callers may inspect the failure
        // value as a published node pointer before retrying.
        unsafe { self.pointer_at(level) }.compare_exchange(
            expected,
            new,
            Ordering::Release,
            Ordering::Acquire,
        )
    }
}

impl<T> CollectionNode<T> for SkipTrieNode<T> {
    fn key(&self) -> &T {
        self.key()
    }

    /// SkipTrieNode uses flexible-array allocation — NOT Box.
    unsafe fn dealloc_ptr(ptr: *mut Self) {
        // SAFETY: Caller guarantees `ptr` was allocated by `alloc_with_key` or
        // `alloc_sentinel`, the node is fully unlinked, and this is the only
        // deallocation call. Delegates to `dealloc_node`.
        unsafe { Self::dealloc_node(ptr) }
    }
}

// =============================================================================
// SkipTriePosition
// =============================================================================

/// Position handle for SortedCollection trait.
///
/// Stores predecessors at all truncated skiplist levels for O(1) amortized
/// batch operations. When threaded through `insert_batch`, each subsequent
/// insert starts from the previous position's predecessors instead of HEAD,
/// giving O(1) amortized per element for sorted data.
pub struct SkipTriePosition<T> {
    /// Predecessors at all levels for O(1) batch operations
    preds: [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
    /// Current node at this position
    node: *mut SkipTrieNode<T>,
    /// Number of valid preds entries (0..valid_height are valid).
    /// Entries at or above this index may be uninitialized.
    valid_height: usize,
}

// No Copy/Clone: positions hold raw pointers and must not outlive
// the guard epoch in which they were created (move-only).

impl<T> SkipTriePosition<T> {
    /// Create a position with predecessors from a completed traversal.
    fn new(
        preds: [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
        node: SkipTrieNodePtr<T>,
        valid_height: usize,
    ) -> Self {
        SkipTriePosition {
            preds,
            node,
            valid_height,
        }
    }

    /// Get the predecessors array and valid height for hint threading.
    #[allow(dead_code)]
    fn preds(&self) -> (&[SkipTrieNodePtr<T>; MAX_TRIE_LEVELS], usize) {
        (&self.preds, self.valid_height)
    }
}

impl<T> NodePosition<T> for SkipTriePosition<T> {
    type Node = SkipTrieNode<T>;

    fn node(&self) -> Option<*mut Self::Node> {
        if self.node.is_null() {
            None
        } else {
            Some(self.node)
        }
    }

    fn empty() -> Self {
        SkipTriePosition {
            preds: [ptr::null_mut(); MAX_TRIE_LEVELS],
            node: ptr::null_mut(),
            valid_height: 0,
        }
    }

    fn from_node(node: *mut Self::Node) -> Self {
        SkipTriePosition {
            preds: [ptr::null_mut(); MAX_TRIE_LEVELS],
            node,
            valid_height: 0,
        }
    }
}

// =============================================================================
// TruncatedSkipList
// =============================================================================

/// The truncated skiplist component with height capped at log(log(u)).
///
/// Uses Harris-style pointer marking for lock-free deletion: a node is
/// logically deleted by CAS-marking its next pointers (top level down to
/// level 0). The level-0 mark is the linearization point. Physical unlinking
/// is cooperative — traversals help CAS-unlink marked nodes they encounter.
struct TruncatedSkipList<T> {
    head: AtomicPtr<SkipTrieNode<T>>,
    max_height: usize,
}

impl<T: Ord + Default> TruncatedSkipList<T> {
    fn new(key_universe_size: usize) -> Self {
        let max_height = log_log_u(key_universe_size);
        let head_ptr = SkipTrieNode::alloc_sentinel(max_height);

        TruncatedSkipList {
            head: AtomicPtr::new(head_ptr),
            max_height,
        }
    }

    /// Generate random height using bit manipulation for efficiency.
    /// Single `fastrand` call + trailing-zeros count, matching SkipList::random_level().
    fn random_height(&self) -> usize {
        let random_bits = fastrand::u32(..);
        let extra_levels = (!random_bits).trailing_zeros() as usize;
        extra_levels.min(self.max_height)
    }

    /// Recover a valid predecessor at the given level by walking UP through
    /// saved predecessors from higher levels. O(1) amortized, matching the
    /// SkipList's recovery strategy instead of restarting from HEAD.
    fn recover_pred(&self, level: usize, preds: &[SkipTrieNodePtr<T>]) -> SkipTrieNodePtr<T> {
        let head = self.head.load(Ordering::Acquire);
        for &pred in preds.iter().skip(level + 1) {
            if pred.is_null() {
                break;
            }
            if pred == head {
                return head;
            }
            // SAFETY: `pred` comes from the preds[] array, which stores pointers to
            // live skiplist nodes obtained during traversal. Nodes are not freed while
            // any thread holds the guard, so dereferencing is valid.
            unsafe {
                let pred_next_0 = (*pred).get_next(0);
                if !MarkedPtr::new(pred_next_0).is_any_marked() && (*pred).height() >= level {
                    return pred;
                }
            }
        }
        head
    }

    /// CAS-unlink a marked node. On CAS failure, recover pred from higher levels
    /// instead of restarting the entire traversal from HEAD.
    unsafe fn snip_marked_node(
        &self,
        level: usize,
        pred: SkipTrieNodePtr<T>,
        curr: SkipTrieNodePtr<T>,
        next_raw: *mut SkipTrieNode<T>,
        preds: &[SkipTrieNodePtr<T>],
    ) -> (SkipTrieNodePtr<T>, SkipTrieNodePtr<T>) {
        // SAFETY: Caller (find_position_from / find_at_level) guarantees `pred` and `curr`
        // are valid skiplist node pointers obtained during traversal within the guard.
        unsafe {
            let clean_next = MarkedPtr::new(next_raw).as_ptr();
            if (*pred).cas_next(level, curr, clean_next).is_ok() {
                (pred, clean_next)
            } else {
                let new_pred = self.recover_pred(level, preds);
                let new_curr = MarkedPtr::new((*new_pred).get_next(level)).as_ptr();
                (new_pred, new_curr)
            }
        }
    }

    /// Unified top-down traversal filling preds and succs arrays where
    /// preds[level].key < key <= succs[level].key at each level.
    /// Uses recover_pred for O(1) amortized recovery from deleted/updated predecessors.
    ///
    /// Two hint mechanisms (mutually exclusive in practice):
    /// - `hint`: single node from x-fast trie (O(log log u) random access)
    /// - `start_preds`: predecessor array from previous position (O(1) amortized batch)
    ///
    /// When `start_preds` is provided, each level validates and uses the hint
    /// predecessor at that level, giving O(1) amortized for sorted batch inserts.
    fn find_position_from<Q>(
        &self,
        key: &Q,
        hint: Option<*mut SkipTrieNode<T>>,
        start_preds: Option<(&[SkipTrieNodePtr<T>; MAX_TRIE_LEVELS], usize)>,
        preds: &mut [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
        succs: &mut [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
    ) where
        T: Borrow<Q>,
        Q: Ord,
    {
        // SAFETY: `head` is valid for the lifetime of the TruncatedSkipList (allocated in new(),
        // freed in Drop). Hint and traversed node pointers are live skiplist nodes protected by
        // memory reclamation — nodes are not freed while any thread holds a guard.
        unsafe {
            let head = self.head.load(Ordering::Acquire);

            // Determine starting point from hint
            let (mut last_pred, start_level) = match hint {
                Some(node)
                    if !node.is_null()
                        && !(*node).is_logically_deleted()
                        && !(*node).is_sentinel()
                        && (*node).key().borrow() < key =>
                {
                    let h = std::cmp::min((*node).height(), self.max_height);
                    (node, h)
                }
                _ => (head, self.max_height),
            };

            // Fill upper levels above start_level with head (conservative)
            for level in (start_level + 1..=self.max_height).rev() {
                preds[level] = head;
                succs[level] = ptr::null_mut();
            }

            for level in (0..=start_level).rev() {
                // Try to use provided predecessor at this level for O(1) batch optimization.
                // SAFETY: `hint_pred` comes from a prior find_position_from preds array,
                // guard-protected. We read atomic fields (next, height) and immutable key.
                if let Some((hint_preds, valid_height)) = start_preds
                    && level < valid_height
                {
                    let hint_pred = hint_preds[level];
                    if !hint_pred.is_null() {
                        let hint_next = (*hint_pred).get_next(level);
                        let is_marked = MarkedPtr::new(hint_next).is_any_marked();
                        let key_ok =
                            (*hint_pred).is_sentinel() || (*hint_pred).key().borrow() < key;
                        if !is_marked && key_ok && (*hint_pred).height() > level {
                            last_pred = hint_pred;
                        }
                    }
                }

                let mut pred = last_pred;

                // Load pred.next[level]. If any-marked, recover via higher-level preds.
                let mut curr = {
                    let pred_next = (*pred).get_next(level);
                    if MarkedPtr::new(pred_next).is_any_marked() {
                        pred = self.recover_pred(level, preds);
                        MarkedPtr::new((*pred).get_next(level)).as_ptr()
                    } else {
                        MarkedPtr::new(pred_next).as_ptr()
                    }
                };

                // Horizontal traversal at this level
                while !curr.is_null() {
                    let succ_raw = (*curr).get_next(level);

                    if MarkedPtr::new(succ_raw).is_any_marked() {
                        (pred, curr) = self.snip_marked_node(level, pred, curr, succ_raw, preds);
                        continue;
                    }

                    if (*curr).key().borrow() >= key {
                        break;
                    }

                    pred = curr;
                    curr = MarkedPtr::new(succ_raw).as_ptr();
                }

                last_pred = pred;
                preds[level] = pred;
                succs[level] = curr;
            }
        }
    }

    /// Re-search at a single level starting from a recovered predecessor.
    /// Used when insert CAS fails at a specific level.
    unsafe fn find_at_level<Q>(
        &self,
        key: &Q,
        level: usize,
        preds: &mut [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
        succs: &mut [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
    ) where
        T: Borrow<Q>,
        Q: Ord,
    {
        // SAFETY: recover_pred returns a valid predecessor node (either head or a live node
        // from preds[]). Caller must hold a pinned guard so traversed nodes are not reclaimed.
        unsafe {
            let mut pred = self.recover_pred(level, preds);
            let mut curr = MarkedPtr::new((*pred).get_next(level)).as_ptr();

            while !curr.is_null() {
                let succ_raw = (*curr).get_next(level);

                if MarkedPtr::new(succ_raw).is_any_marked() {
                    (pred, curr) = self.snip_marked_node(level, pred, curr, succ_raw, preds);
                    continue;
                }

                if (*curr).key().borrow() >= key {
                    break;
                }

                pred = curr;
                curr = MarkedPtr::new(succ_raw).as_ptr();
            }

            preds[level] = pred;
            succs[level] = curr;
        }
    }

    /// Insert a key into the truncated skiplist.
    /// Returns the node pointer if inserted, None if key already exists.
    fn insert(&self, key: T, hint: Option<*mut SkipTrieNode<T>>) -> Option<SkipTrieNodePtr<T>> {
        let mut out_preds = [ptr::null_mut(); MAX_TRIE_LEVELS];
        self.insert_from(key, hint, None, &mut out_preds)
            .map(|(node, _)| node)
    }

    /// Insert with position hint support. Accepts `start_preds` from a previous
    /// position for O(1) amortized batch inserts. Writes the traversal's
    /// predecessors into `out_preds` and returns `(node, valid_height)`.
    fn insert_from(
        &self,
        key: T,
        hint: Option<*mut SkipTrieNode<T>>,
        start_preds: Option<(&[SkipTrieNodePtr<T>; MAX_TRIE_LEVELS], usize)>,
        out_preds: &mut [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
    ) -> Option<(SkipTrieNodePtr<T>, usize)> {
        let height = self.random_height();
        // RAII-owned until the level-0 publish CAS. A panic in a user `Eq`/`Ord`
        // comparison run below (find_position_from, the level-0 dup-check, find_at_level
        // re-search) would otherwise leak this unpublished allocation. The node is a
        // flexible-array-member allocation, so its deallocator is `dealloc_node` — `Box`
        // cannot free it. Freed IMMEDIATELY on drop, never epoch-deferred: an
        // unpublished node is thread-private.
        let new_ptr = SkipTrieNode::alloc_with_key(key, height);
        // SAFETY: `alloc_with_key` yields the sole owning pointer to a fresh,
        // unpublished node, and `dealloc_node` is its matching (FAM) deallocator.
        let mut guard =
            Some(unsafe { UnpublishedNode::new(new_ptr, SkipTrieNode::<T>::dealloc_node) });

        let mut succs = [ptr::null_mut(); MAX_TRIE_LEVELS];
        // SAFETY: `new_ptr` was just created via `alloc_with_key` above and has not
        // been freed or shared yet, so dereferencing it to read the key is valid.
        unsafe {
            self.find_position_from((*new_ptr).key(), hint, start_preds, out_preds, &mut succs);
        };

        // Insert from bottom up using cached positions
        for level in 0..=height {
            loop {
                let prev = out_preds[level];
                let curr = succs[level];

                // SAFETY: `new_ptr` is exclusively owned by this thread until CAS-linked
                // (`guard` frees it on any early exit / unwind before the level-0 publish).
                // `prev` and `curr` are live skiplist nodes from find_position_from,
                // protected by memory reclamation.
                unsafe {
                    // Check for duplicate at level 0
                    if level == 0 && !curr.is_null() && *(*curr).key() == *(*new_ptr).key() {
                        return None; // `guard` (still Some) drops here, freeing the node.
                    }

                    // For levels > 0, use CAS instead of plain store to set
                    // next[level]. A concurrent delete/update marks next pointers
                    // top-down, so there's a window where next[level] is already
                    // marked but next[0] isn't yet. CAS detects the mark.
                    if level > 0 {
                        let old_next = (*new_ptr).get_next(level);
                        if MarkedPtr::new(old_next).is_any_marked() {
                            return Some((new_ptr, self.max_height + 1));
                        }
                        if (*new_ptr).cas_next(level, old_next, curr).is_err() {
                            // Marked between our load and CAS — stop raising
                            return Some((new_ptr, self.max_height + 1));
                        }
                    } else {
                        (*new_ptr).set_next_relaxed(0, curr);
                    }

                    if (*prev).cas_next(level, curr, new_ptr).is_ok() {
                        // Published at level 0 (now reachable): relinquish the guard so
                        // it does not free the now-live node. Higher levels link an
                        // already-published node, so a panic there cannot leak it.
                        if level == 0 {
                            guard.take().map(UnpublishedNode::publish);
                        }
                        break;
                    }
                    // CAS failed — re-search this level using recovery
                    self.find_at_level((*new_ptr).key(), level, out_preds, &mut succs);
                }
            }
        }

        Some((new_ptr, self.max_height + 1))
    }

    /// Find predecessor of a key (largest key <= target).
    /// Uses unified top-down traversal with O(1) amortized recovery.
    fn find_predecessor<Q>(
        &self,
        key: &Q,
        hint: Option<*mut SkipTrieNode<T>>,
    ) -> Option<SkipTrieNodePtr<T>>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let mut out_preds = [ptr::null_mut(); MAX_TRIE_LEVELS];
        self.find_predecessor_from(key, hint, None, &mut out_preds)
            .map(|(node, _)| node)
    }

    /// Find predecessor with position hint support.
    /// Returns `(node, valid_height)`.
    fn find_predecessor_from<Q>(
        &self,
        key: &Q,
        hint: Option<*mut SkipTrieNode<T>>,
        start_preds: Option<(&[SkipTrieNodePtr<T>; MAX_TRIE_LEVELS], usize)>,
        out_preds: &mut [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
    ) -> Option<(SkipTrieNodePtr<T>, usize)>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let head = self.head.load(Ordering::Acquire);
        let mut succs = [ptr::null_mut(); MAX_TRIE_LEVELS];
        self.find_position_from(key, hint, start_preds, out_preds, &mut succs);

        let valid_height = self.max_height + 1;

        // succs[0] is first node with key >= target — check for exact match
        if !succs[0].is_null() {
            // SAFETY: succs[0] was set by find_position_from to a non-null live skiplist
            // node. Null is excluded by the enclosing check. Node is guard-protected.
            unsafe {
                if (*succs[0]).key().borrow() == key && !(*succs[0]).is_logically_deleted() {
                    return Some((succs[0], valid_height));
                }
            }
        }

        // preds[0] is last node with key < target
        let pred = out_preds[0];
        // SAFETY: `pred` is a live skiplist node from find_position_from (either head
        // or a data node). The `pred != head` check is done first; the deref only
        // executes for non-head nodes which are guard-protected.
        if pred == head || unsafe { (*pred).is_sentinel() } {
            None
        } else {
            Some((pred, valid_height))
        }
    }

    /// Find the strict predecessor of a key (largest key < target).
    /// Uses unified top-down traversal with O(1) amortized recovery.
    fn find_strict_predecessor<Q>(
        &self,
        key: &Q,
        hint: Option<*mut SkipTrieNode<T>>,
    ) -> Option<SkipTrieNodePtr<T>>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let mut out_preds = [ptr::null_mut(); MAX_TRIE_LEVELS];
        self.find_strict_predecessor_from(key, hint, None, &mut out_preds)
            .map(|(node, _)| node)
    }

    /// Find strict predecessor with position hint support.
    /// Returns `(node, valid_height)`.
    fn find_strict_predecessor_from<Q>(
        &self,
        key: &Q,
        hint: Option<*mut SkipTrieNode<T>>,
        start_preds: Option<(&[SkipTrieNodePtr<T>; MAX_TRIE_LEVELS], usize)>,
        out_preds: &mut [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
    ) -> Option<(SkipTrieNodePtr<T>, usize)>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let head = self.head.load(Ordering::Acquire);
        let mut succs = [ptr::null_mut(); MAX_TRIE_LEVELS];
        self.find_position_from(key, hint, start_preds, out_preds, &mut succs);

        let pred = out_preds[0];
        let valid_height = self.max_height + 1;
        // SAFETY: `pred` is a live skiplist node from find_position_from (either head
        // or a data node). The `pred != head` check is done first; the deref only
        // executes for non-head nodes which are guard-protected.
        if pred == head || unsafe { (*pred).is_sentinel() } {
            None
        } else {
            Some((pred, valid_height))
        }
    }

    /// Find the node with an exact key match, if it exists and is not logically deleted.
    /// Uses `Borrow<Q>` to allow searching by a borrowed form of the key (e.g., `&K`
    /// when `T = Pair<K, V>`).
    fn find_node_internal<Q>(
        &self,
        key: &Q,
        hint: Option<*mut SkipTrieNode<T>>,
    ) -> Option<*mut SkipTrieNode<T>>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let mut preds = [ptr::null_mut(); MAX_TRIE_LEVELS];
        let mut succs = [ptr::null_mut(); MAX_TRIE_LEVELS];
        self.find_position_from(key, hint, None, &mut preds, &mut succs);

        if !succs[0].is_null() {
            // SAFETY: succs[0] is non-null (checked above) and was set by find_position_from
            // to a live skiplist node. Node is guard-protected.
            unsafe {
                if (*succs[0]).key().borrow() == key && !(*succs[0]).is_logically_deleted() {
                    return Some(succs[0]);
                }
            }
        }
        None
    }

    /// Logically delete a key by marking next pointers, then physically unlink.
    ///
    /// Protocol (Harris-style):
    /// 1. Find node via find_position_from (get preds/succs at all levels)
    /// 2. Mark next pointers from top level down to level 1
    /// 3. Mark next[0] (linearization point — the node is now logically deleted)
    /// 4. Physically unlink at all levels via unlink_at_level (guaranteed)
    ///
    /// Step 4 uses unlink_at_level (which loops until the CAS succeeds or
    /// the node is already unlinked) instead of best-effort find_position_from
    /// cooperative help. This guarantees the node is physically unreachable
    /// before the caller defers deallocation — preventing use-after-free
    /// when epoch-based reclamation frees the node.
    ///
    /// Returns the node pointer if successfully marked (caller defers dealloc).
    fn delete<Q>(&self, key: &Q, hint: Option<*mut SkipTrieNode<T>>) -> Option<*mut SkipTrieNode<T>>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let mut out_preds = [ptr::null_mut(); MAX_TRIE_LEVELS];
        self.delete_from(key, hint, None, &mut out_preds)
            .map(|(node, _)| node)
    }

    /// Delete with position hint support.
    /// Returns `(node, valid_height)`.
    fn delete_from<Q>(
        &self,
        key: &Q,
        hint: Option<*mut SkipTrieNode<T>>,
        start_preds: Option<(&[SkipTrieNodePtr<T>; MAX_TRIE_LEVELS], usize)>,
        out_preds: &mut [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
    ) -> Option<(*mut SkipTrieNode<T>, usize)>
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let mut succs = [ptr::null_mut(); MAX_TRIE_LEVELS];
        self.find_position_from(key, hint, start_preds, out_preds, &mut succs);

        let valid_height = self.max_height + 1;

        if succs[0].is_null() {
            return None;
        }

        // SAFETY: succs[0] is non-null (checked above) and was set by find_position_from
        // to a live skiplist node. All node dereferences within this block access nodes
        // that are guard-protected and not yet freed.
        unsafe {
            let curr = succs[0];
            if (*curr).is_sentinel() || (*curr).key().borrow() != key {
                return None;
            }

            // Already being deleted or updated?
            let next0 = (*curr).get_next(0);
            if MarkedPtr::new(next0).is_any_marked() {
                return None;
            }

            let height = (*curr).height();

            // Check if node is fully linked at all its levels.
            // Same race as update: a concurrent insert still linking higher
            // levels could create a zombie node if we proceed too early.
            if height > 0 && succs[height] != curr {
                return None;
            }

            // Mark next pointers from top level down to level 1
            for level in (1..=height).rev() {
                loop {
                    let next = (*curr).get_next(level);
                    let next_marked = MarkedPtr::new(next);

                    if next_marked.is_any_marked() {
                        break; // Already marked (DELETE or UPDATE)
                    }

                    let marked = next_marked.with_mark(true);
                    if (*curr).cas_next_weak(level, next, marked.as_raw()).is_ok() {
                        break;
                    }
                }
            }

            // Mark level 0 — this is the linearization point.
            // Check is_any_marked: both DELETE and UPDATE marks mean
            // another thread owns this node (first to mark level 0 wins).
            let we_own = loop {
                let next = (*curr).get_next(0);
                let next_marked = MarkedPtr::new(next);

                if next_marked.is_any_marked() {
                    break false;
                }

                let marked = next_marked.with_mark(true);
                if (*curr).cas_next_weak(0, next, marked.as_raw()).is_ok() {
                    break true;
                }
            };

            if !we_own {
                return None;
            }

            // Physically unlink at all levels (guaranteed, not best-effort).
            // unlink_at_level loops until the CAS succeeds or the node is
            // already unlinked, ensuring the node is fully unreachable before
            // the caller defers deallocation via epoch-based reclamation.
            for level in (0..=height).rev() {
                let mut pred = out_preds[level];
                self.unlink_at_level(level, &mut pred, curr, key, None, out_preds);
            }

            Some((curr, valid_height))
        }
    }

    // =========================================================================
    // Per-level insert/unlink helpers (used by update protocol)
    // =========================================================================

    /// Insert a new node at a specific level.
    ///
    /// Handles:
    /// - pred.next changed (concurrent insert — advance pred if smaller key)
    /// - new_node concurrently marked (deleted/updated — bail out)
    /// - pred itself marked (recover from higher levels)
    ///
    /// Returns true if successfully linked, false if new_node was marked.
    unsafe fn insert_at_level<Q>(
        &self,
        level: usize,
        pred: &mut SkipTrieNodePtr<T>,
        new_node: SkipTrieNodePtr<T>,
        node_key: &Q,
        preds: &[SkipTrieNodePtr<T>],
    ) -> bool
    where
        T: Borrow<Q>,
        Q: Ord,
    {
        let head = self.head.load(Ordering::Acquire);
        // SAFETY: Caller guarantees `new_node` is a valid pointer to a node created via
        // `alloc_with_key`. `pred` is a live skiplist node from preds[]. `head` is valid
        // for the list's lifetime. All traversed nodes are guard-protected.
        unsafe {
            loop {
                // Check if new_node is being deleted/updated at level 0
                let node_next_0 = (*new_node).get_next(0);
                if MarkedPtr::new(node_next_0).is_any_marked() {
                    return false;
                }

                // Check if pred itself is being deleted/updated (level 0 marked).
                if *pred != head {
                    let pred_next_0 = (*(*pred)).get_next(0);
                    if MarkedPtr::new(pred_next_0).is_any_marked() {
                        *pred = self.recover_pred(level, preds);
                        continue;
                    }
                }

                let pred_next = (*(*pred)).get_next(level);
                let pred_next_marked = MarkedPtr::new(pred_next);
                let pred_next_ptr = pred_next_marked.as_ptr();

                // If pred.next is marked at THIS level, recover from level+1
                if pred_next_marked.is_any_marked() {
                    *pred = self.recover_pred(level, preds);
                    continue;
                }

                // Advance pred if needed (concurrent insert of smaller key)
                if !pred_next_ptr.is_null()
                    && !(*pred_next_ptr).is_sentinel()
                    && (*pred_next_ptr).height() >= level
                    && (*pred_next_ptr).key().borrow() < node_key
                {
                    let next_0 = (*pred_next_ptr).get_next(0);
                    if MarkedPtr::new(next_0).is_any_marked() {
                        continue;
                    }
                    *pred = pred_next_ptr;
                    continue;
                }

                // If pred.next is already new_node, we're done
                if pred_next_ptr == new_node {
                    return true;
                }

                // Set new_node.next[level] = pred.next[level].
                // CRITICAL: Use CAS instead of plain store to detect concurrent
                // DELETE_MARK set by another thread updating/deleting new_node.
                // A plain store (set_next_relaxed) can overwrite a concurrent mark,
                // causing the node to be linked at this level but never cleaned up
                // (zombie node → use-after-free when epoch reclamation frees it).
                {
                    let old_next = (*new_node).get_next(level);
                    if MarkedPtr::new(old_next).is_any_marked() {
                        return false;
                    }
                    if (*new_node)
                        .cas_next_weak(level, old_next, pred_next_ptr)
                        .is_err()
                    {
                        // Marked between load and CAS
                        return false;
                    }
                }

                // CAS pred.next from pred_next_ptr to new_node
                match (*(*pred)).cas_next(level, pred_next_ptr, new_node) {
                    Ok(_) => return true,
                    Err(_) => continue,
                }
            }
        }
    }

    /// Unlink a node at a specific level with full retry logic.
    ///
    /// If `replacement` is provided, use it instead of `node.next[level]`.
    /// This is used for UPDATE at level 0 where replacement = new_node.
    ///
    /// Returns when node is unlinked at this level (or already was).
    unsafe fn unlink_at_level<Q>(
        &self,
        level: usize,
        pred: &mut SkipTrieNodePtr<T>,
        node: SkipTrieNodePtr<T>,
        node_key: &Q,
        replacement: Option<SkipTrieNodePtr<T>>,
        preds: &[SkipTrieNodePtr<T>],
    ) where
        T: Borrow<Q>,
        Q: Ord,
    {
        // SAFETY: Caller guarantees `pred` points to a valid live skiplist node and
        // `node` is the marked node being unlinked. All traversed nodes (including
        // replacement targets) are guard-protected and not freed during this operation.
        unsafe {
            loop {
                let pred_next = (*(*pred)).get_next(level);
                let pred_next_marked = MarkedPtr::new(pred_next);
                let pred_next_ptr = pred_next_marked.as_ptr();

                // A MARKED slot means PRED ITSELF is dead at this level; its frozen
                // value is a stale snapshot and proves NOTHING about `node` (it can
                // sort strictly past `node` while `node` is still linked elsewhere —
                // concluding from it retires a reachable node: use-after-free; see
                // the identical SkipList fix / `test_unlink_ignores_dead_pred_*`).
                // This check must precede every conclusion below.
                if pred_next_marked.is_any_marked() && pred_next_ptr != node {
                    *pred = self.recover_pred(level, preds);
                    continue;
                }

                if pred_next_ptr != node {
                    // pred.next != node (CLEAN slot): either unlinked or concurrent
                    // insert happened. STRICTLY-greater only: an EQUAL key may be a
                    // same-key UPDATE replacement standing in FRONT of `node` while
                    // `node` is still linked behind it — advance through it.
                    if pred_next_ptr.is_null()
                        || (*pred_next_ptr).is_sentinel()
                        || (*pred_next_ptr).key().borrow() > node_key
                    {
                        return; // Already unlinked
                    }
                    // Concurrent insert happened, advance pred
                    // At level 0, follow through UPDATE-marked nodes
                    let mut new_pred = pred_next_ptr;
                    if level == 0 {
                        loop {
                            let new_pred_next = (*new_pred).get_next(0);
                            let new_pred_next_marked = MarkedPtr::new(new_pred_next);
                            if !new_pred_next_marked.is_update_marked() {
                                break;
                            }
                            new_pred = new_pred_next_marked.as_ptr();
                        }
                    }
                    *pred = new_pred;
                    continue;
                }

                // pred.next == node, check if pred is marked
                if pred_next_marked.is_any_marked() {
                    *pred = self.recover_pred(level, preds);
                    continue;
                }

                // Compute replacement
                let replacement_ptr = replacement.unwrap_or_else(|| {
                    let node_next = (*node).get_next(level);
                    MarkedPtr::unmask(node_next)
                });

                // Try to unlink: CAS pred.next from node to replacement
                match (*(*pred)).cas_next(level, node, replacement_ptr) {
                    Ok(_) => return,
                    Err(actual) => {
                        // Same rule as the loop top: a MARKED actual means pred died
                        // under us — its frozen value proves nothing.
                        if MarkedPtr::new(actual).is_any_marked() {
                            *pred = self.recover_pred(level, preds);
                            continue;
                        }
                        let actual_ptr = MarkedPtr::unmask(actual);
                        if actual_ptr == replacement_ptr {
                            return; // Already unlinked with this replacement
                        }
                        // CLEAN slot; strictly-greater only (equal keys advanced through).
                        if actual_ptr.is_null()
                            || (*actual_ptr).is_sentinel()
                            || (*actual_ptr).key().borrow() > node_key
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

    // =========================================================================
    // Atomic update (mark-then-insert protocol)
    // =========================================================================

    /// Atomically replace an existing node with a new node containing `new_value`.
    ///
    /// Uses the same mark-then-insert protocol as SkipList:
    ///
    /// 1. Find position via find_position_from (get preds/succs)
    /// 2. Verify succs[0] is the target, not already marked
    /// 3. Unlink higher levels (top-down marking)
    /// 4. Create new_node, CAS curr.next[0]: succ → new_node|UPDATE_MARK
    ///    (single CAS — atomically marks curr and links new_node)
    /// 5. Link new_node at higher levels
    /// 6. Unlink curr at level 0
    ///
    /// Returns (old_node, new_node) on success, None if key not found.
    #[allow(dead_code)]
    fn update(
        &self,
        key: T,
        hint: Option<*mut SkipTrieNode<T>>,
    ) -> Option<(SkipTrieNodePtr<T>, SkipTrieNodePtr<T>)> {
        let mut out_preds = [ptr::null_mut(); MAX_TRIE_LEVELS];
        self.update_from(key, hint, None, &mut out_preds)
            .map(|(result, _)| result)
    }

    /// Update with position hint support.
    /// Returns `((old_node, new_node), valid_height)`.
    #[allow(clippy::type_complexity)]
    fn update_from(
        &self,
        key: T,
        hint: Option<*mut SkipTrieNode<T>>,
        start_preds: Option<(&[SkipTrieNodePtr<T>; MAX_TRIE_LEVELS], usize)>,
        out_preds: &mut [SkipTrieNodePtr<T>; MAX_TRIE_LEVELS],
    ) -> Option<((SkipTrieNodePtr<T>, SkipTrieNodePtr<T>), usize)> {
        let mut succs = [ptr::null_mut(); MAX_TRIE_LEVELS];
        self.find_position_from(&key, hint, start_preds, out_preds, &mut succs);

        let valid_height = self.max_height + 1;

        if succs[0].is_null() {
            return None;
        }

        // SAFETY: succs[0] is non-null (checked above) and was set by find_position_from
        // to a live skiplist node. `new_node` is created via `alloc_with_key` and exclusively
        // owned until CAS-linked. `dealloc_node` on failure reclaims our exclusive allocation.
        // All traversed nodes are guard-protected.
        unsafe {
            let curr = succs[0];
            if (*curr).is_sentinel() || *(*curr).key() != key {
                return None;
            }

            // Check if node is already being deleted/updated (level 0 marked).
            // This early check avoids marking higher levels only to discover
            // at the CAS that someone else owns the node.
            {
                let next0 = (*curr).get_next(0);
                if MarkedPtr::new(next0).is_any_marked() {
                    return None;
                }
            }

            let height = (*curr).height();

            // Check if node is fully linked at all its levels.
            // A node being inserted bottom-up may be at level 0 but not yet
            // linked at its top level. Updating such a node races with the
            // inserter — skip it and let the caller retry.
            if height > 0 && succs[height] != curr {
                return None;
            }

            // Step 1: Unlink higher levels first (top-down)
            for level in (1..=height).rev() {
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
                let mut pred = out_preds[level];
                self.unlink_at_level(level, &mut pred, curr, &key, None, out_preds);
            }

            // Step 2+3: Create new_node and forward-insert atomically.
            //
            // CAS curr.next[0]: succ → new_node|UPDATE_MARK
            // This single CAS atomically marks curr as being updated AND links
            // new_node after it, so the key is never absent from the list.
            let new_node = SkipTrieNode::alloc_with_key(key, height);

            let we_own = loop {
                let next = (*curr).get_next(0);
                let next_marked = MarkedPtr::new(next);

                if next_marked.is_any_marked() {
                    break false;
                }

                // Set new_node.next[0] = current successor (reload each retry)
                (*new_node).set_next_relaxed(0, next);

                // CAS: curr.next[0]: succ → new_node|UPDATE_MARK
                let marked_new = MarkedPtr::new(new_node).with_update_mark(true);
                if (*curr).cas_next_weak(0, next, marked_new.as_raw()).is_ok() {
                    break true;
                }
            };

            if !we_own {
                // CAS failed — another thread owns this node (DELETE or UPDATE)
                SkipTrieNode::dealloc_node(new_node);
                return None;
            }

            // Step 4: Link new_node at higher levels
            for level in 1..=height {
                let mut pred = out_preds[level];
                if !self.insert_at_level(level, &mut pred, new_node, (*new_node).key(), out_preds) {
                    break;
                }
            }

            // Step 5: Unlink curr at level 0
            // CAS preds[0].next: curr → new_node
            let mut pred = out_preds[0];
            self.unlink_at_level(
                0,
                &mut pred,
                curr,
                (*new_node).key(),
                Some(new_node),
                out_preds,
            );

            Some(((curr, new_node), valid_height))
        }
    }

    /// Check if a node reached the top level.
    fn is_top_level(&self, node: *mut SkipTrieNode<T>) -> bool {
        // SAFETY: `node` was returned by insert/delete/update, which guarantee a valid
        // pointer. The `height` field is immutable after construction, so reading it
        // on a marked (logically deleted) node is safe.
        unsafe { (*node).height() == self.max_height }
    }

    /// Get the first data node (after sentinel), skipping deleted/updated nodes.
    ///
    /// Follows UPDATE-marked pointers to replacement nodes (the key is still
    /// present — the old node chains to the new one via UPDATE_MARK).
    fn first_node(&self) -> Option<*mut SkipTrieNode<T>> {
        // SAFETY: `head` is valid for the list's lifetime. Traversed nodes are reached
        // via atomic loads on next pointers and are guard-protected (not freed while
        // any thread holds a guard).
        unsafe {
            let head = self.head.load(Ordering::Acquire);
            let mut curr = (*head).get_next(0);

            while !curr.is_null() {
                let next = (*curr).get_next(0);
                let next_marked = MarkedPtr::new(next);

                if next_marked.is_update_marked() && !next_marked.is_delete_marked() {
                    // UPDATE-marked: follow to replacement node.
                    // Don't return immediately — replacement might be
                    // concurrently deleted, so continue the loop.
                    curr = next_marked.as_ptr();
                    continue;
                }

                if !next_marked.is_any_marked() {
                    return Some(curr);
                }

                // DELETE-marked: skip to next
                curr = next_marked.as_ptr();
            }

            None
        }
    }

    /// Get the next unmarked node after the given node, following UPDATE chains.
    fn next_node(&self, node: *mut SkipTrieNode<T>) -> Option<*mut SkipTrieNode<T>> {
        let node = MarkedPtr::unmask(node);
        if node.is_null() {
            return None;
        }

        // SAFETY: `node` was validated non-null above, and came from a previous
        // first_node/next_node/insert/find call. Traversed successors are reached via
        // atomic next-pointer loads and are guard-protected.
        unsafe {
            // Key just yielded for `node`. A forward-insertion UPDATE links `node`'s NEW
            // same-key replacement immediately after it; returning that replacement would
            // yield the key TWICE. Keys are unique, so any node reached here whose key
            // equals `node`'s is a transient UPDATE replacement — skip it (this also covers
            // update-during-update chains), along with any DELETE/UPDATE-marked node. The
            // genuine successor has a strictly greater key.
            // Sentinels (the truncated skip list's head) carry NO key — the
            // SortedList analog of this read panicked on the below-minimum
            // `range(start..)` predecessor position; guard identically.
            let node_key = if (*node).is_sentinel() {
                None
            } else {
                Some((*node).key())
            };
            let mut curr = MarkedPtr::unmask((*node).get_next(0));

            while !curr.is_null() {
                let next_marked = MarkedPtr::new((*curr).get_next(0));

                if !next_marked.is_any_marked() && node_key.is_none_or(|k| (*curr).key() != k) {
                    return Some(curr);
                }

                curr = next_marked.as_ptr();
            }

            None
        }
    }
}

// =============================================================================
// XFastTrie
// =============================================================================

/// X-fast trie over the top-level skiplist nodes, backed by an open-addressing
/// PrefixTable and an optional per-level PrefixBitmap for fast existence checks.
///
/// Uses order-preserving key bits (via `KeyBits` trait) — not hash bits — so the
/// trie's binary search on prefix length correctly reflects key ordering.
///
/// Prefix keys are encoded as `u64` using a sentinel-bit scheme:
/// `(1 << len) | (bits >> (key_bits - len))`. The leading 1-bit
/// distinguishes prefixes of different lengths (e.g., prefix "01" vs "1").
struct XFastTrie<T> {
    prefix_table: PrefixTable<T>,
    bitmap: Option<PrefixBitmap>,
    key_bits: usize,
}

impl<T: KeyBits> XFastTrie<T> {
    fn new(key_universe_size: usize) -> Self {
        // key_bits = ceil_log2(universe_size), capped at 63 so the sentinel
        // bit in encode_prefix fits in u64 (1u64 << 63 is valid, << 64 overflows).
        // For keys in [0, u), the bottom ceil_log2(u) bits of to_bits() equal
        // the key values directly and preserve order.
        let key_bits = if key_universe_size <= 1 {
            1
        } else {
            let raw = (usize::BITS - (key_universe_size - 1).leading_zeros()) as usize;
            std::cmp::min(raw, 63)
        };
        let max_height = log_log_u(key_universe_size);
        XFastTrie {
            prefix_table: PrefixTable::new(key_bits, max_height),
            bitmap: PrefixBitmap::new(key_bits),
            key_bits,
        }
    }

    /// Extract the bottom `key_bits` from the order-preserving bit representation.
    ///
    /// For positive keys in `[0, u)`, `to_bits()` gives `[offset, offset+u)` and
    /// the bottom `ceil_log2(u)` bits exactly equal the key values, preserving order.
    #[inline]
    fn key_to_bits<Q: KeyBits>(&self, key: &Q) -> u64 {
        let bits = key.to_bits();
        if self.key_bits >= 64 {
            bits
        } else {
            bits & ((1u64 << self.key_bits) - 1)
        }
    }

    /// Encode a prefix of `len` bits as a unique `u64` key.
    ///
    /// Uses a sentinel 1-bit above the prefix bits:
    ///   `(1 << len) | (bits >> (key_bits - len))`
    ///
    /// Examples (key_bits=4, bits=0b1010):
    ///   len=0 → 0b1         (root)
    ///   len=1 → 0b11        (prefix "1")
    ///   len=2 → 0b110       (prefix "10")
    ///   len=4 → 0b11010     (leaf)
    #[inline]
    fn encode_prefix(bits: u64, len: usize, key_bits: usize) -> u64 {
        if len == 0 {
            1
        } else {
            (1u64 << len) | (bits >> (key_bits - len))
        }
    }

    /// Get the bit at position `pos` (0 = MSB) within the key_bits-wide value.
    /// Returns 0 if pos >= key_bits.
    #[inline]
    fn bit_at(bits: u64, pos: usize, key_bits: usize) -> usize {
        if pos < key_bits {
            ((bits >> (key_bits - 1 - pos)) & 1) as usize
        } else {
            0
        }
    }

    /// Find the best known pointer into the skiplist for the given key,
    /// using binary search on prefix length (Algorithm 3 from paper).
    fn find_lowest_ancestor<Q>(&self, key: &Q) -> Option<*mut SkipTrieNode<T>>
    where
        T: Borrow<Q>,
        Q: KeyBits + Ord,
    {
        let bits = self.key_to_bits(key);
        let key_bits = self.key_bits;
        let mut best_node: *mut SkipTrieNode<T> = ptr::null_mut();
        let mut start = 0;
        let mut size = key_bits / 2;

        while size > 0 {
            let mid = start + size;
            let prefix_bits_raw = if mid == 0 {
                0
            } else {
                bits >> (key_bits - mid)
            };

            let maybe_exists = match self.bitmap {
                Some(ref bm) => bm.test(mid, prefix_bits_raw),
                None => true, // no bitmap: always probe table
            };

            if maybe_exists {
                let prefix_key = Self::encode_prefix(bits, mid, key_bits);
                if let Some(slot) = self.prefix_table.find(prefix_key) {
                    let direction = Self::bit_at(bits, mid, key_bits);
                    let candidate = slot.pointers[direction].load(Ordering::Acquire);

                    if !candidate.is_null() {
                        start = mid; // advance binary search

                        // Track as predecessor only if key < target
                        // SAFETY: `candidate` is non-null (checked above) and was loaded
                        // via Acquire from PrefixTable, which stores pointers to live
                        // top-level skiplist nodes. Nodes are guard-protected.
                        unsafe {
                            if (*candidate).key().borrow() < key
                                && (best_node.is_null()
                                    || *(*candidate).key() > *(*best_node).key())
                            {
                                best_node = candidate;
                            }
                        }
                    }

                    // Check OTHER direction for a better predecessor.
                    // When direction=1: pointers[0] = max of left subtree, guaranteed < key.
                    // When direction=0: pointers[1] = min of right subtree, guaranteed > key.
                    let other = slot.pointers[1 - direction].load(Ordering::Acquire);
                    // SAFETY: `other` is checked non-null before deref. Loaded via Acquire
                    // from PrefixTable, which stores pointers to live top-level skiplist
                    // nodes. `best_node` is either null or a previously validated pointer.
                    unsafe {
                        if !other.is_null()
                            && (*other).key().borrow() < key
                            && (best_node.is_null() || *(*other).key() > *(*best_node).key())
                        {
                            best_node = other;
                        }
                    }
                }
            }

            size /= 2;
        }

        // Fallback: when no predecessor was found from the binary search levels
        // (all directions were 0 → other-direction pointers were all > key),
        // probe NEIGHBORING prefixes at the deepest found level.  A prefix P-k
        // at level L has strictly smaller keys than K (since K's top-L bits = P
        // and P-k < P), so any pointer there is a valid predecessor.
        //
        // Why 4 probes: with ~62.5K top-level nodes over ~64K 16-bit prefixes,
        // each prefix has ~1 node on average (Poisson λ≈1).  The probability
        // of 4 consecutive empty prefixes is e^(-1)^4 ≈ 1.8%, so this covers
        // >98% of cases.  Failures fall back to HEAD — correct but slower.
        if best_node.is_null() && start > 0 {
            let prefix_bits = bits >> (key_bits - start);
            for k in 1..=4u64 {
                if prefix_bits < k {
                    break; // no more valid neighbors
                }
                let neighbor_prefix = prefix_bits - k;
                // Use bitmap to skip empty neighbors cheaply (when available)
                let maybe_exists = match self.bitmap {
                    Some(ref bm) => bm.test(start, neighbor_prefix),
                    None => true,
                };
                if !maybe_exists {
                    continue;
                }
                // encode_prefix takes full key bits; we have pre-extracted prefix
                // bits, so apply the sentinel-bit encoding directly:
                // (1 << len) | prefix_value  (see encode_prefix, line 1277).
                let neighbor_key = (1u64 << start) | neighbor_prefix;
                if let Some(slot) = self.prefix_table.find(neighbor_key) {
                    // Both pointers are guaranteed < key; pick the max (closest)
                    for d in 0..2 {
                        let candidate = slot.pointers[d].load(Ordering::Acquire);
                        // SAFETY: `candidate` is checked non-null before deref. Loaded via
                        // Acquire from PrefixTable, pointing to a live top-level skiplist
                        // node. `best_node` is either null or a previously validated pointer.
                        unsafe {
                            if !candidate.is_null()
                                && (best_node.is_null()
                                    || *(*candidate).key() > *(*best_node).key())
                            {
                                best_node = candidate;
                            }
                        }
                    }
                    if !best_node.is_null() {
                        break; // found a predecessor
                    }
                }
            }
        }

        if best_node.is_null() {
            None
        } else {
            Some(best_node)
        }
    }

    /// Insert a top-level skiplist node into the trie (all its prefixes).
    ///
    /// Checks `is_any_marked()` (not just DELETE_MARK) because a node that has
    /// been UPDATE-marked will be deferred for destruction — writing its pointer
    /// into the PrefixTable would create a dangling reference once the node is
    /// freed in a later epoch. When a concurrent mark is detected mid-loop,
    /// `remove_node` cleans up any entries already written.
    fn insert_node(&self, node: *mut SkipTrieNode<T>) {
        // SAFETY: `node` is a live top-level skiplist node returned by insert/update.
        // Its key and next pointers are valid to read. The node is guard-protected and
        // will not be freed while this operation is in progress.
        unsafe {
            let bits = self.key_to_bits((*node).key());
            let key_bits = self.key_bits;

            for i in (0..=key_bits).rev() {
                // Must check ANY mark (DELETE or UPDATE), not just DELETE.
                // An UPDATE-marked node will be defer_destroy'd — its pointer
                // must not linger in PrefixTable past its epoch lifetime.
                let next0 = (*node).get_next(0);
                if MarkedPtr::new(next0).is_any_marked() {
                    // Clean up entries written in higher-level iterations
                    self.remove_node(node);
                    return;
                }

                let prefix_key = Self::encode_prefix(bits, i, key_bits);
                let direction = Self::bit_at(bits, i, key_bits);

                let slot = match self.prefix_table.find_or_insert(prefix_key) {
                    Some(s) => s,
                    None => continue, // table full, skip this prefix level
                };
                loop {
                    let current = slot.pointers[direction].load(Ordering::Acquire);
                    let should_update = if current.is_null() {
                        true
                    } else if direction == 0 {
                        *(*node).key() >= *(*current).key()
                    } else {
                        *(*node).key() <= *(*current).key()
                    };
                    if !should_update {
                        break;
                    }
                    if slot.pointers[direction]
                        .compare_exchange_weak(current, node, Ordering::Release, Ordering::Acquire)
                        .is_ok()
                    {
                        break;
                    }
                }

                if let Some(ref bm) = self.bitmap {
                    let prefix_bits_raw = if i == 0 { 0 } else { bits >> (key_bits - i) };
                    bm.set(i, prefix_bits_raw);
                }
            }

            // TOCTOU guard: the node could have been marked after the last
            // in-loop check but before we returned. Clean up if so.
            let next0 = (*node).get_next(0);
            if MarkedPtr::new(next0).is_any_marked() {
                self.remove_node(node);
            }
        }
    }

    /// Remove a node's entries from the trie.
    ///
    /// CAS-nulls the pointer in each prefix slot. The slot itself stays
    /// (insert-only table, no physical deletion). Bitmap bits are never
    /// cleared — stale bits cause at most one extra hash lookup.
    fn remove_node(&self, node: *mut SkipTrieNode<T>) {
        // SAFETY: `node` is a live skiplist node (marked for deletion but not yet freed).
        // Its key field is immutable and safe to read on marked nodes. The node remains
        // valid until memory reclamation frees it after all guards are dropped.
        unsafe {
            let bits = self.key_to_bits((*node).key());
            let key_bits = self.key_bits;

            for i in 0..=key_bits {
                let prefix_key = Self::encode_prefix(bits, i, key_bits);
                let direction = Self::bit_at(bits, i, key_bits);

                if let Some(slot) = self.prefix_table.find(prefix_key) {
                    let _ = slot.pointers[direction].compare_exchange(
                        node,
                        ptr::null_mut(),
                        Ordering::Release,
                        Ordering::Relaxed,
                    );
                }
            }
        }
    }
}

// =============================================================================
// SkipTrie
// =============================================================================

/// A concurrent sorted set based on the SkipTrie data structure
/// (Oshman & Shavit, PODC 2013).
///
/// Combines a truncated skiplist of height log(log(u)) with an x-fast trie
/// for O(log log u) amortized predecessor queries, where u is the key universe size.
///
/// Implements `SortedCollection<T>` for integration with the crate's collection framework.
pub struct SkipTrie<T, G: Guard> {
    skiplist: TruncatedSkipList<T>,
    x_fast_trie: XFastTrie<T>,
    guard: G,
    count: AtomicUsize,
    key_universe_size: usize,
}

impl<T: Eq + Ord + KeyBits + Clone + Default, G: Guard> SkipTrie<T, G> {
    /// Create a new SkipTrie with the default key universe size (2^30).
    ///
    /// Uses truncated skiplist height (log log u ≈ 4-5 levels) with the
    /// x-fast trie providing O(log log u) starting positions for every
    /// operation (Oshman & Shavit, PODC 2013).
    pub fn new() -> Self {
        Self::with_universe_size(MAX_U)
    }

    /// Create a new SkipTrie with specified key universe size.
    ///
    /// The skiplist height is truncated to log(log(u)) levels and the x-fast
    /// trie is always active, providing amortized expected O(log log u) operations.
    ///
    /// **Trade-off:** Each top-level node (1/2^height chance) requires ~key_bits
    /// hash table operations for trie maintenance. With truncated height this is
    /// a small constant cost per operation.
    pub fn with_universe_size(key_universe_size: usize) -> Self {
        SkipTrie {
            skiplist: TruncatedSkipList::new(key_universe_size),
            x_fast_trie: XFastTrie::new(key_universe_size),
            guard: G::default(),
            count: AtomicUsize::new(0),
            key_universe_size,
        }
    }

    /// Get the key universe size.
    pub fn universe_size(&self) -> usize {
        self.key_universe_size
    }

    /// Get the height of the truncated skiplist (log log u).
    pub fn skiplist_height(&self) -> usize {
        self.skiplist.max_height
    }

    /// Find the predecessor of a key (largest key <= target).
    ///
    /// This is the primary operation the SkipTrie is optimized for,
    /// providing amortized expected O(log log u) performance.
    ///
    /// Returns a guarded reference to the predecessor key. The reference
    /// owns a guard internally, keeping the node alive until dropped.
    pub fn predecessor(&self, key: &T) -> Option<G::GuardedRef<'_, T>> {
        let _guard = G::pin();
        let hint = self.x_fast_trie.find_lowest_ancestor(key);

        if let Some(node) = self.skiplist.find_predecessor(key, hint) {
            // SAFETY: `node` was returned by find_predecessor, which guarantees a valid
            // non-null pointer to a live skiplist node. The guard `_guard` keeps
            // the node alive for the duration of this scope.
            unsafe {
                if !(*node).is_logically_deleted() {
                    return Some(G::make_ref((*node).key()));
                }
            }
        }

        None
    }

    /// Find the strict predecessor of a key (largest key < target).
    ///
    /// Returns a guarded reference to the predecessor key. The reference
    /// owns a guard internally, keeping the node alive until dropped.
    pub fn strict_predecessor(&self, key: &T) -> Option<G::GuardedRef<'_, T>> {
        let _guard = G::pin();
        let hint = self.x_fast_trie.find_lowest_ancestor(key);

        if let Some(node) = self.skiplist.find_strict_predecessor(key, hint) {
            // SAFETY: `node` was returned by find_strict_predecessor, which guarantees
            // a valid non-null pointer to a live skiplist node. The guard `_guard`
            // keeps the node alive for the duration of this scope.
            unsafe {
                if !(*node).is_logically_deleted() {
                    return Some(G::make_ref((*node).key()));
                }
            }
        }

        None
    }

    /// Get the number of elements.
    pub fn element_count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Choose between position-based preds (batch) and x-fast trie hint (random).
    ///
    /// When a position with valid preds is available, copies them and skips the
    /// x-fast trie lookup. Otherwise falls back to the O(log log u) trie lookup.
    ///
    /// # Safety (caller contract)
    ///
    /// The raw pointers in `position.preds` must remain valid — the caller must
    /// hold an epoch guard that was active when the position was created.
    /// Positions must not outlive their originating epoch (same contract as
    /// `SkipNodePosition` in SkipList). `find_position_from` re-validates each
    /// hint pointer before use (marked-check, key-order, height).
    fn resolve_hints(
        x_fast_trie: &XFastTrie<T>,
        key: &T,
        position: Option<&SkipTriePosition<T>>,
    ) -> (Option<*mut SkipTrieNode<T>>, Option<SkipTriePreds<T>>) {
        match position {
            Some(pos) if pos.valid_height > 0 => (None, Some((pos.preds, pos.valid_height))),
            _ => (x_fast_trie.find_lowest_ancestor(key), None),
        }
    }
}

impl<T: Eq + Ord + KeyBits + Clone + Default, G: Guard> Default for SkipTrie<T, G> {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// SortedCollection implementation
// =============================================================================

impl<T: Eq + Ord + KeyBits + Clone + Default, G: Guard> SortedCollectionInternal<T>
    for SkipTrie<T, G>
{
    type Guard = G;
    type Node = SkipTrieNode<T>;
    type NodePosition = SkipTriePosition<T>;

    fn guard(&self) -> &G {
        &self.guard
    }

    unsafe fn insert_from_internal(
        &self,
        key: T,
        position: Option<&Self::NodePosition>,
    ) -> Option<Self::NodePosition> {
        let mut out_preds = [ptr::null_mut(); MAX_TRIE_LEVELS];

        // Use preds from previous position for O(1) amortized batch inserts,
        // or fall back to x-fast trie lookup for random access.
        let (hint, start_preds) = Self::resolve_hints(&self.x_fast_trie, &key, position);

        let (node, valid_height) = self.skiplist.insert_from(
            key,
            hint,
            start_preds.as_ref().map(|(p, vh)| (p, *vh)),
            &mut out_preds,
        )?;

        self.count.fetch_add(1, Ordering::Relaxed);

        // If node reached top level, insert into x-fast trie
        if self.skiplist.is_top_level(node) {
            self.x_fast_trie.insert_node(node);
        }

        Some(SkipTriePosition::new(out_preds, node, valid_height))
    }

    unsafe fn remove_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
    ) -> Option<Self::NodePosition> {
        let mut out_preds = [ptr::null_mut(); MAX_TRIE_LEVELS];

        let (hint, start_preds) = Self::resolve_hints(&self.x_fast_trie, key, position);

        let (node, valid_height) = self.skiplist.delete_from(
            key,
            hint,
            start_preds.as_ref().map(|(p, vh)| (p, *vh)),
            &mut out_preds,
        )?;

        self.count.fetch_sub(1, Ordering::Relaxed);

        // Check is_top_level on the deleted node itself (marked but still valid,
        // height is immutable). This avoids the TOCTOU race of checking before delete.
        if self.skiplist.is_top_level(node) {
            self.x_fast_trie.remove_node(node);
        }

        Some(SkipTriePosition::new(out_preds, node, valid_height))
    }

    unsafe fn find_from_internal(
        &self,
        position: Option<&Self::NodePosition>,
        key: &T,
        is_match: bool,
    ) -> Option<Self::NodePosition> {
        let mut out_preds = [ptr::null_mut(); MAX_TRIE_LEVELS];

        let (hint, start_preds) = Self::resolve_hints(&self.x_fast_trie, key, position);

        if is_match {
            // Exact match: find_predecessor_from returns largest key <= target
            let (node, valid_height) = self.skiplist.find_predecessor_from(
                key,
                hint,
                start_preds.as_ref().map(|(p, vh)| (p, *vh)),
                &mut out_preds,
            )?;
            // SAFETY: `node` was returned by find_predecessor_from, guaranteeing a valid
            // non-null pointer to a live skiplist node protected by memory reclamation.
            unsafe {
                if *(*node).key() == *key && !(*node).is_logically_deleted() {
                    Some(SkipTriePosition::new(out_preds, node, valid_height))
                } else {
                    None
                }
            }
        } else {
            // Predecessor position: return the largest node with key < target
            let (node, valid_height) = self.skiplist.find_strict_predecessor_from(
                key,
                hint,
                start_preds.as_ref().map(|(p, vh)| (p, *vh)),
                &mut out_preds,
            )?;
            // SAFETY: `node` was returned by find_strict_predecessor_from, guaranteeing a
            // valid non-null pointer to a live skiplist node protected by memory reclamation.
            unsafe {
                if !(*node).is_logically_deleted() {
                    Some(SkipTriePosition::new(out_preds, node, valid_height))
                } else {
                    None
                }
            }
        }
    }

    unsafe fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        if node.is_null() {
            return None;
        }
        // SAFETY: caller guarantees `node` is valid (from insert/find/first/next).
        unsafe {
            if (*node).is_sentinel() {
                return None;
            }
            Some(f((*node).key()))
        }
    }

    unsafe fn first_node_internal(&self) -> Option<*mut Self::Node> {
        self.skiplist.first_node()
    }

    unsafe fn next_node_internal(&self, node: *mut Self::Node) -> Option<*mut Self::Node> {
        self.skiplist.next_node(node)
    }

    unsafe fn update_internal(
        &self,
        position: Option<&Self::NodePosition>,
        new_value: T,
    ) -> Option<Self::NodePosition> {
        let mut out_preds = [ptr::null_mut(); MAX_TRIE_LEVELS];

        // Atomic mark-then-insert protocol: the key is never absent from the
        // list. TruncatedSkipList::update atomically marks the old node with
        // UPDATE_MARK and links the new node in a single CAS.
        let (hint, start_preds) = Self::resolve_hints(&self.x_fast_trie, &new_value, position);

        let ((old_node, new_node), valid_height) = self.skiplist.update_from(
            new_value,
            hint,
            start_preds.as_ref().map(|(p, vh)| (p, *vh)),
            &mut out_preds,
        )?;

        // X-fast trie maintenance: insert new node first, then remove old.
        // Both checks use the node's immutable height field (safe on marked nodes).
        if self.skiplist.is_top_level(new_node) {
            self.x_fast_trie.insert_node(new_node);
        }
        if self.skiplist.is_top_level(old_node) {
            self.x_fast_trie.remove_node(old_node);
        }

        // RETIREMENT (internal, per the trait contract): `update_from` fully
        // unlinked the old node across every skip-list level, and the x-fast trie
        // reference was removed above — it is unreachable, so deferring its
        // destruction here is epoch-safe. Callers never retire.
        // SAFETY: old node unlinked + unreferenced as argued above.
        unsafe {
            self.guard
                .defer_destroy(old_node, <Self::Node as CollectionNode<T>>::dealloc_ptr);
        }

        Some(SkipTriePosition::new(out_preds, new_node, valid_height))
    }
}

impl<T: Eq + Ord + KeyBits + Clone + Default, G: Guard> SortedCollection<T> for SkipTrie<T, G> {}

// =============================================================================
// Drop
// =============================================================================

impl<T, G: Guard> Drop for SkipTrie<T, G> {
    fn drop(&mut self) {
        // SAFETY: `drop` has exclusive access (`&mut self`), so no concurrent readers
        // exist. `head` was allocated via `alloc_sentinel` in TruncatedSkipList::new.
        // Each node in the level-0 chain was allocated via `alloc_with_key` during
        // insert. `dealloc_node` reclaims each node exactly once by following the chain.
        unsafe {
            let head = self.skiplist.head.load(Ordering::Acquire);
            if !head.is_null() {
                // Follow level-0 chain, stripping all marks (DELETE + UPDATE).
                // Both old (UPDATE-marked) and new (replacement) nodes are in
                // the chain, so both get visited and freed.
                let mut curr = MarkedPtr::new((*head).get_next(0)).as_ptr();

                while !curr.is_null() {
                    let next_raw = (*curr).get_next(0);
                    let next_marked = MarkedPtr::new(next_raw);

                    // INVARIANT CHECK: no DELETE-marked nodes should remain
                    // at drop time — they should have been physically unlinked.
                    // UPDATE-only marked nodes are allowed (old→new chain).
                    if next_marked.is_delete_marked() && !(*curr).is_sentinel() {
                        panic!(
                            "INVARIANT VIOLATION: Found DELETE-marked node at drop time!\n\
                             Marked nodes should have been physically unlinked before drop."
                        );
                    }

                    let next = next_marked.as_ptr();
                    SkipTrieNode::dealloc_node(curr);
                    curr = next;
                }

                SkipTrieNode::dealloc_node(head);
            }
        }
    }
}

// =============================================================================
// Send / Sync
// =============================================================================

unsafe impl<T: Send, G: Guard> Send for SkipTrie<T, G> {}
unsafe impl<T: Send + Sync, G: Guard> Sync for SkipTrie<T, G> {}

// =============================================================================
// MapCollection — key-value map semantics via Pair<K, V>
// =============================================================================

impl<K: Eq, V> MapNode<K, V> for SkipTrieNode<Pair<K, V>> {
    #[inline]
    fn key(&self) -> &K {
        &CollectionNode::key(self).key
    }

    #[inline]
    fn value(&self) -> Option<&V> {
        Some(&CollectionNode::key(self).value)
    }

    unsafe fn dealloc_ptr(ptr: *mut Self) {
        // SAFETY: Caller guarantees `ptr` is a valid node allocated via `alloc_with_key`
        // and is no longer referenced.
        unsafe { Self::dealloc_node(ptr) }
    }
}

/// Note: `V: Default` is required because `SkipTrie`'s sentinel node uses `T::default()`
/// (i.e. `Pair<K,V>::default()`). This is stricter than `SkipList`/`SortedList` impls which
/// do not require `Default` on `V`.
impl<K, V, G: Guard> MapCollectionInternal<K, V> for SkipTrie<Pair<K, V>, G>
where
    K: Eq + Ord + KeyBits + Clone + Default,
    V: Clone + Default,
{
    type Guard = G;
    type Node = SkipTrieNode<Pair<K, V>>;

    fn guard(&self) -> &Self::Guard {
        &self.guard
    }

    fn insert_internal(&self, key: K, value: V) -> Option<*mut Self::Node> {
        let hint = self.x_fast_trie.find_lowest_ancestor(&key);
        let node = self.skiplist.insert(Pair::new(key, value), hint)?;

        self.count.fetch_add(1, Ordering::Relaxed);

        if self.skiplist.is_top_level(node) {
            self.x_fast_trie.insert_node(node);
        }

        Some(node)
    }

    fn remove_internal(&self, key: &K) -> Option<*mut Self::Node> {
        let hint = self.x_fast_trie.find_lowest_ancestor(key);
        let node = self.skiplist.delete(key, hint)?;

        self.count.fetch_sub(1, Ordering::Relaxed);

        if self.skiplist.is_top_level(node) {
            // SAFETY: `node` was just returned by `delete`, which guarantees a valid
            // marked node. The key is immutable, so reading it is safe.
            self.x_fast_trie.remove_node(node);
        }

        Some(node)
    }

    fn find_internal(&self, key: &K) -> Option<*mut Self::Node> {
        let hint = self.x_fast_trie.find_lowest_ancestor(key);
        self.skiplist.find_node_internal(key, hint)
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
        if node.is_null() {
            return None;
        }
        // SAFETY: `node` is non-null (checked above). Caller guarantees it was returned
        // by insert_internal/find_internal, so it points to a valid, guard-protected node.
        // Caller must hold a pinned epoch guard for the duration of this call.
        unsafe {
            if (*node).is_sentinel() {
                return None;
            }
            let pair = CollectionNode::key(&*node);
            Some(f(&pair.key, &pair.value))
        }
    }

    fn update_internal(&self, key: K, value: V) -> bool {
        // Delegates to the node-replacement update_internal with Q = Pair<K,V> (not
        // Q = K like insert/remove/find). Correct because Pair<K,V>::to_bits()
        // delegates to K::to_bits(), producing identical bit encodings for the
        // x-fast trie lookup.
        // SAFETY: guard pinned by caller (MapCollection::update). The replaced node
        // is unlinked AND retired internally by the node-replacement update.
        unsafe {
            SortedCollectionInternal::update_internal(self, None, Pair::new(key, value)).is_some()
        }
    }

    fn len_internal(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

impl<K, V, G: Guard> MapCollection<K, V> for SkipTrie<Pair<K, V>, G>
where
    K: Eq + Ord + KeyBits + Clone + Default,
    V: Clone + Default,
{
}

// =============================================================================
// PrefixSlot — single entry in the open-addressing PrefixTable
// =============================================================================

/// A slot in the PrefixTable. Stores a prefix key and two pointers into the
/// top-level skiplist (left/right child subtree boundaries).
///
/// 24 bytes per slot (~2.6 slots per 64-byte cache line). Linear probing
/// touches adjacent cache lines, so the hardware prefetcher keeps up.
#[repr(C)]
struct PrefixSlot<T> {
    /// Encoded prefix key. 0 = empty (valid keys are always >= 1 due to
    /// the sentinel bit in `encode_prefix`).
    key: AtomicU64,
    /// pointers[0] = largest node in 0-subtree (left).
    /// pointers[1] = smallest node in 1-subtree (right).
    pointers: [AtomicPtr<SkipTrieNode<T>>; 2],
}

impl<T> PrefixSlot<T> {
    const EMPTY_KEY: u64 = 0;
}

// =============================================================================
// PrefixTable — lock-free open-addressing hash table for XFastTrie prefixes
// =============================================================================

/// Fixed-capacity, insert-only, lock-free hash table for XFastTrie prefix entries.
///
/// - Keys: u64 encoded prefixes (always >= 1).
/// - EMPTY_KEY = 0 sentinel for unoccupied slots.
/// - Linear probing with power-of-2 capacity.
/// - No physical deletion: `remove_node` CAS-nulls pointers but the slot stays.
/// - No epoch-based reclamation: slots live for the table's lifetime.
struct PrefixTable<T> {
    slots: Box<[PrefixSlot<T>]>,
    mask: usize, // capacity - 1
}

impl<T> PrefixTable<T> {
    /// FxHash constant for u64 keys (from rustc-hash).
    const FX_SEED: u64 = 0x517cc1b727220a95;

    fn new(key_bits: usize, max_height: usize) -> Self {
        // Size the table for ~50% load at the expected number of unique prefix
        // entries.  Each top-level node (probability 1/2^max_height) creates
        // up to key_bits+1 prefix entries, but shallow levels are shared across
        // nodes.  Unique entries ≈ N_top * (key_bits - log2(N_top) + 1) where
        // N_top = 2^(key_bits.min(20) - max_height).
        let top_exp = key_bits.min(20).saturating_sub(max_height);
        let expected_top_nodes = 1usize << top_exp;
        let prefix_depth = key_bits.saturating_sub(top_exp) + 1;
        let estimated_entries = expected_top_nodes.saturating_mul(prefix_depth);
        let capacity = estimated_entries
            .saturating_mul(2) // target ~50% load factor
            .checked_next_power_of_two()
            .unwrap_or(1 << 21)
            .clamp(4096, 1 << 21); // cap at 2M slots = 48 MB
        debug_assert!(capacity.is_power_of_two());

        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(PrefixSlot {
                key: AtomicU64::new(PrefixSlot::<T>::EMPTY_KEY),
                pointers: [
                    AtomicPtr::new(ptr::null_mut()),
                    AtomicPtr::new(ptr::null_mut()),
                ],
            });
        }

        PrefixTable {
            slots: slots.into_boxed_slice(),
            mask: capacity - 1,
        }
    }

    #[inline]
    fn hash_index(&self, prefix_key: u64) -> usize {
        let h = prefix_key.wrapping_mul(Self::FX_SEED);
        (h >> 32) as usize & self.mask
    }

    /// Find a slot by prefix key. Returns `None` if the key is not in the table
    /// or the table is full (all slots probed without finding the key).
    fn find(&self, prefix_key: u64) -> Option<&PrefixSlot<T>> {
        debug_assert!(prefix_key != PrefixSlot::<T>::EMPTY_KEY);
        let mut idx = self.hash_index(prefix_key);
        for _ in 0..self.mask + 1 {
            let slot = &self.slots[idx];
            let k = slot.key.load(Ordering::Acquire);
            if k == prefix_key {
                return Some(slot);
            }
            if k == PrefixSlot::<T>::EMPTY_KEY {
                return None;
            }
            idx = (idx + 1) & self.mask;
        }
        None
    }

    /// Find existing slot or claim a new one. Returns `None` if the table is
    /// full (all slots occupied by other keys). Concurrent inserts of the same
    /// key converge to one slot (CAS ensures only one winner).
    fn find_or_insert(&self, prefix_key: u64) -> Option<&PrefixSlot<T>> {
        debug_assert!(prefix_key != PrefixSlot::<T>::EMPTY_KEY);
        let mut idx = self.hash_index(prefix_key);
        for _ in 0..self.mask + 1 {
            let slot = &self.slots[idx];
            let k = slot.key.load(Ordering::Acquire);
            if k == prefix_key {
                return Some(slot);
            }
            if k == PrefixSlot::<T>::EMPTY_KEY {
                match slot.key.compare_exchange(
                    PrefixSlot::<T>::EMPTY_KEY,
                    prefix_key,
                    Ordering::Release,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(slot),
                    Err(actual) if actual == prefix_key => return Some(slot),
                    Err(_) => { /* different key claimed it, continue probing */ }
                }
            }
            idx = (idx + 1) & self.mask;
        }
        None
    }
}

// =============================================================================
// PrefixBitmap — per-level existence bitmap for fast find_lowest_ancestor
// =============================================================================

/// Per-level bitmap for fast existence checks in `find_lowest_ancestor`.
///
/// `levels[L]` has `ceil(2^L / 64)` AtomicU64 words. A set bit means the
/// prefix has been inserted into the PrefixTable.
///
/// - Only allocated when key_bits <= 24 (above that, bitmap exceeds 4 MB).
/// - Set with `fetch_or` during `insert_node`. Never cleared.
/// - Stale bits (from deleted prefixes) cause at most one extra hash lookup.
struct PrefixBitmap {
    levels: Vec<Vec<AtomicU64>>,
}

impl PrefixBitmap {
    fn new(key_bits: usize) -> Option<Self> {
        if key_bits > 24 {
            return None;
        }
        let mut levels = Vec::with_capacity(key_bits + 1);
        for level in 0..=key_bits {
            let num_prefixes = 1usize << level;
            let num_words = num_prefixes.div_ceil(64);
            let mut words = Vec::with_capacity(num_words);
            for _ in 0..num_words {
                words.push(AtomicU64::new(0));
            }
            levels.push(words);
        }
        Some(PrefixBitmap { levels })
    }

    /// Test whether the prefix at (level, prefix_bits) might exist.
    #[inline]
    fn test(&self, level: usize, prefix_bits: u64) -> bool {
        let word_idx = (prefix_bits / 64) as usize;
        let bit_idx = prefix_bits % 64;
        let word = self.levels[level][word_idx].load(Ordering::Relaxed);
        (word >> bit_idx) & 1 != 0
    }

    /// Mark the prefix at (level, prefix_bits) as present.
    #[inline]
    fn set(&self, level: usize, prefix_bits: u64) {
        let word_idx = (prefix_bits / 64) as usize;
        let bit_idx = prefix_bits % 64;
        self.levels[level][word_idx].fetch_or(1u64 << bit_idx, Ordering::Relaxed);
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_structures::SortedCollection;
    use crate::guard::DeferredGuard;
    use std::sync::Arc;
    use std::thread;

    // Positions must be move-only to prevent stale pointer reuse across guard epochs.
    static_assertions::assert_not_impl_any!(SkipTriePosition<i32>: Copy, Clone);

    #[test]
    fn test_range_start_below_minimum_does_not_panic() {
        // Below-minimum-range regression: the below-minimum `range(start..)` shape
        // hands the head-sentinel predecessor position to `next_node`, which
        // must not read the sentinel's (absent) key.
        let trie: SkipTrie<i32, DeferredGuard> = SkipTrie::new();
        trie.insert(10);
        trie.insert(20);
        let v: Vec<i32> = trie.range(5..15).map(|x| *x).collect();
        assert_eq!(v, vec![10]);
        let all: Vec<i32> = trie.range(0..).map(|x| *x).collect();
        assert_eq!(all, vec![10, 20]);
    }

    type TestSkipTrie = SkipTrie<i32, DeferredGuard>;

    #[test]
    fn test_prefix_table_capacity_sizing() {
        // Test the actual PrefixTable::new code path, not a mirrored formula.
        fn cap(key_bits: usize, max_height: usize) -> usize {
            PrefixTable::<i32>::new(key_bits, max_height).mask + 1
        }

        // key_bits=30, max_height=4 (default MAX_U): ~983K entries → 2M at 50% load
        assert_eq!(cap(30, 4), 1 << 21); // 2M

        // key_bits=30, max_height=5: ~524K entries → 1M at 50% load
        assert_eq!(cap(30, 5), 1 << 20); // 1M

        // key_bits=30, max_height=20: ~31 entries → minimum 4096
        assert_eq!(cap(30, 20), 4096);

        // key_bits=10, max_height=3: ~512 entries → minimum 4096
        assert_eq!(cap(10, 3), 4096);

        // key_bits=20, max_height=4: ~328K entries → 1M at 50% load (below 2M cap)
        assert_eq!(cap(20, 4), 1 << 20); // 1M
    }

    #[test]
    fn test_basic_insert_and_find() {
        let trie = TestSkipTrie::new();

        assert!(trie.insert(5));
        assert!(trie.insert(3));
        assert!(trie.insert(7));
        assert!(trie.insert(1));

        assert!(trie.contains(&5));
        assert!(trie.contains(&3));
        assert!(trie.contains(&7));
        assert!(trie.contains(&1));
        assert!(!trie.contains(&10));

        assert_eq!(trie.element_count(), 4);
    }

    #[test]
    fn test_duplicate_insert() {
        let trie = TestSkipTrie::new();

        assert!(trie.insert(5));
        assert!(!trie.insert(5));

        assert_eq!(trie.element_count(), 1);
    }

    #[test]
    fn test_delete() {
        let trie = TestSkipTrie::new();

        trie.insert(1);
        trie.insert(2);
        trie.insert(3);

        assert!(trie.delete(&2));
        assert!(!trie.contains(&2));
        assert_eq!(trie.element_count(), 2);

        assert!(!trie.delete(&2)); // Already deleted
    }

    #[test]
    fn test_predecessor() {
        let trie = TestSkipTrie::new();

        trie.insert(1);
        trie.insert(3);
        trie.insert(5);
        trie.insert(7);

        // Exact matches
        assert_eq!(*trie.predecessor(&5).unwrap(), 5);
        assert_eq!(*trie.predecessor(&1).unwrap(), 1);
        assert_eq!(*trie.predecessor(&7).unwrap(), 7);

        // In-between values
        assert_eq!(*trie.predecessor(&4).unwrap(), 3);
        assert_eq!(*trie.predecessor(&6).unwrap(), 5);
        assert_eq!(*trie.predecessor(&8).unwrap(), 7);

        // No predecessor
        assert!(trie.predecessor(&0).is_none());
    }

    #[test]
    fn test_strict_predecessor() {
        let trie = TestSkipTrie::new();

        trie.insert(1);
        trie.insert(3);
        trie.insert(5);

        assert_eq!(*trie.strict_predecessor(&5).unwrap(), 3);
        assert_eq!(*trie.strict_predecessor(&4).unwrap(), 3);
        assert!(trie.strict_predecessor(&1).is_none());
    }

    #[test]
    fn test_sorted_collection_iter() {
        let trie = TestSkipTrie::new();

        trie.insert(5);
        trie.insert(1);
        trie.insert(3);
        trie.insert(7);

        let values: Vec<i32> = trie.iter().map(|item| *item).collect();
        assert_eq!(values, vec![1, 3, 5, 7]);
    }

    #[test]
    fn test_sorted_collection_range() {
        let trie = TestSkipTrie::new();

        for i in 0..10 {
            trie.insert(i);
        }

        let range: Vec<i32> = trie.range(3..7).map(|item| *item).collect();
        assert_eq!(range, vec![3, 4, 5, 6]);
    }

    #[test]
    fn test_concurrent_inserts() {
        let trie = Arc::new(TestSkipTrie::new());
        let mut handles = vec![];

        for i in 0..8 {
            let trie_clone = Arc::clone(&trie);
            let handle = thread::spawn(move || {
                for j in 0..1000 {
                    let key = i * 1000 + j;
                    trie_clone.insert(key);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(trie.element_count(), 8000);

        // Verify some keys exist
        assert!(trie.contains(&0));
        assert!(trie.contains(&50));
        assert!(trie.contains(&7999));
    }

    #[test]
    fn test_empty_trie() {
        let trie = TestSkipTrie::new();

        assert!(trie.is_empty());
        assert_eq!(trie.element_count(), 0);
        assert!(!trie.contains(&1));
        assert!(trie.predecessor(&1).is_none());
    }

    // =========================================================================
    // Update tests
    // =========================================================================

    #[test]
    fn test_basic_update() {
        let trie = TestSkipTrie::new();

        trie.insert(1);
        trie.insert(3);
        trie.insert(5);

        // Update existing key — should succeed
        assert!(trie.update(3));
        assert!(trie.contains(&3));
        assert_eq!(trie.element_count(), 3);

        // Update non-existing key — should fail
        assert!(!trie.update(10));
        assert_eq!(trie.element_count(), 3);
    }

    #[test]
    fn test_update_preserves_iteration_order() {
        let trie = TestSkipTrie::new();

        for i in 1..=10 {
            trie.insert(i);
        }

        // Update several keys
        trie.update(3);
        trie.update(7);
        trie.update(1);

        let values: Vec<i32> = trie.iter().map(|item| *item).collect();
        assert_eq!(values, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(trie.element_count(), 10);
    }

    #[test]
    fn test_update_then_delete() {
        let trie = TestSkipTrie::new();

        trie.insert(1);
        trie.insert(2);
        trie.insert(3);

        assert!(trie.update(2));
        assert!(trie.contains(&2));

        assert!(trie.delete(&2));
        assert!(!trie.contains(&2));
        assert_eq!(trie.element_count(), 2);
    }

    #[test]
    fn test_update_predecessor() {
        let trie = TestSkipTrie::new();

        trie.insert(1);
        trie.insert(5);
        trie.insert(10);

        // Update and verify predecessor still works
        trie.update(5);
        assert_eq!(*trie.predecessor(&5).unwrap(), 5);
        assert_eq!(*trie.predecessor(&7).unwrap(), 5);
        assert_eq!(*trie.predecessor(&4).unwrap(), 1);
    }

    #[test]
    fn test_two_threads_same_key_update() {
        // Minimal same-key contention test: 2 threads, 1 key
        let trie = Arc::new(TestSkipTrie::new());
        trie.insert(42);

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut handles = vec![];

        for _ in 0..2 {
            let trie_clone = Arc::clone(&trie);
            let barrier_clone = Arc::clone(&barrier);
            let handle = thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..100 {
                    trie_clone.update(42);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert!(trie.contains(&42));
        assert_eq!(trie.element_count(), 1);
    }

    #[test]
    fn test_concurrent_updates_same_keys() {
        // Key-always-present regression: concurrent updates should never make a key
        // temporarily invisible (no lost reads).
        let trie = Arc::new(TestSkipTrie::new());

        for i in 0..100 {
            trie.insert(i);
        }

        let num_updaters = 4;
        let barrier = Arc::new(std::sync::Barrier::new(num_updaters + 1));
        let mut handles = vec![];

        // All updater threads update the SAME keys (overlapping ranges).
        for _ in 0..num_updaters {
            let trie_clone = Arc::clone(&trie);
            let barrier_clone = Arc::clone(&barrier);
            let handle = thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..10 {
                    for key in 0..100 {
                        trie_clone.update(key);
                    }
                }
            });
            handles.push(handle);
        }

        // 1 reader thread checking keys are always visible
        let trie_reader = Arc::clone(&trie);
        let barrier_reader = Arc::clone(&barrier);
        let reader = thread::spawn(move || {
            barrier_reader.wait();
            let mut missing_count = 0u64;
            for _ in 0..20 {
                for key in 0..100 {
                    if !trie_reader.contains(&key) {
                        missing_count += 1;
                    }
                }
            }
            missing_count
        });

        for handle in handles {
            handle.join().unwrap();
        }

        let missing = reader.join().unwrap();
        assert_eq!(
            missing, 0,
            "key-always-present regression: {missing} reads found key absent during concurrent updates"
        );

        // All keys should still be present
        assert_eq!(trie.element_count(), 100);
        for i in 0..100 {
            assert!(
                trie.contains(&i),
                "key {i} missing after concurrent updates"
            );
        }
    }

    #[test]
    fn test_concurrent_updates_and_inserts() {
        // Mixed concurrent updates and inserts should not corrupt the structure.
        let trie = Arc::new(TestSkipTrie::new());

        // Pre-insert even keys
        for i in (0..100).step_by(2) {
            trie.insert(i);
        }

        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut handles = vec![];

        // 2 threads updating even keys (disjoint ranges to avoid pathological contention)
        for t in 0..2 {
            let trie_clone = Arc::clone(&trie);
            let barrier_clone = Arc::clone(&barrier);
            let handle = thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..10 {
                    for key in (t * 50..((t + 1) * 50)).step_by(2) {
                        trie_clone.update(key);
                    }
                }
            });
            handles.push(handle);
        }

        // 2 threads inserting odd keys
        for t in 0..2 {
            let trie_clone = Arc::clone(&trie);
            let barrier_clone = Arc::clone(&barrier);
            let handle = thread::spawn(move || {
                barrier_clone.wait();
                for key in (1..100).step_by(2) {
                    let _ = trie_clone.insert(key + t * 1000);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // All even keys must still exist
        for i in (0..100).step_by(2) {
            assert!(
                trie.contains(&i),
                "even key {i} missing after concurrent updates+inserts"
            );
        }
    }

    #[test]
    fn test_concurrent_updates_and_deletes() {
        // Concurrent updates and deletes: only one should succeed per key.
        // The key should either be present (update won) or absent (delete won).
        let trie = Arc::new(TestSkipTrie::new());

        for i in 0..100 {
            trie.insert(i);
        }

        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut handles = vec![];

        // 4 threads updating
        for _ in 0..4 {
            let trie_clone = Arc::clone(&trie);
            let barrier_clone = Arc::clone(&barrier);
            let handle = thread::spawn(move || {
                barrier_clone.wait();
                for key in 0..100 {
                    trie_clone.update(key);
                }
            });
            handles.push(handle);
        }

        // 4 threads deleting
        for _ in 0..4 {
            let trie_clone = Arc::clone(&trie);
            let barrier_clone = Arc::clone(&barrier);
            let handle = thread::spawn(move || {
                barrier_clone.wait();
                for key in 0..100 {
                    trie_clone.delete(&key);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify structural integrity: iteration should produce sorted output
        let values: Vec<i32> = trie.iter().map(|item| *item).collect();
        for window in values.windows(2) {
            assert!(window[0] < window[1], "iteration not sorted: {:?}", values);
        }
        assert_eq!(values.len(), trie.element_count());
    }

    #[test]
    fn test_update_iteration_consistency() {
        // Iteration during updates: output should always be non-decreasing
        // (sorted). Transient duplicates are acceptable — they occur when an
        // iterator traverses an UPDATE-marked node to its same-key replacement
        // before the old node is physically unlinked. This is standard for
        // lock-free iterators that don't provide snapshot isolation.
        let trie = Arc::new(TestSkipTrie::new());

        for i in 0..50 {
            trie.insert(i);
        }

        let barrier = Arc::new(std::sync::Barrier::new(5));
        let mut handles = vec![];

        // 4 update threads
        for _ in 0..4 {
            let trie_clone = Arc::clone(&trie);
            let barrier_clone = Arc::clone(&barrier);
            let handle = thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..10 {
                    for key in 0..50 {
                        trie_clone.update(key);
                    }
                }
            });
            handles.push(handle);
        }

        // 1 iterator thread checking sorted order (non-decreasing)
        let trie_iter = Arc::clone(&trie);
        let barrier_iter = Arc::clone(&barrier);
        let iter_handle = thread::spawn(move || {
            barrier_iter.wait();
            let mut out_of_order = 0u64;
            for _ in 0..100 {
                let values: Vec<i32> = trie_iter.iter().map(|item| *item).collect();
                for window in values.windows(2) {
                    if window[0] > window[1] {
                        out_of_order += 1;
                    }
                }
            }
            out_of_order
        });

        for handle in handles {
            handle.join().unwrap();
        }

        let violations = iter_handle.join().unwrap();
        assert_eq!(
            violations, 0,
            "{violations} ordering violations during iteration with concurrent updates"
        );
    }

    // =========================================================================
    // FAM primitive tests (alloc, dealloc, height_flags packing, pointer_at)
    // =========================================================================

    #[test]
    fn test_height_flags_roundtrip() {
        // Verify height/sentinel packing for data and sentinel nodes at
        // various heights including boundary values.
        for height in [0, 1, 5, 20, 127] {
            unsafe {
                // Data node: sentinel flag must be clear
                let data = SkipTrieNode::alloc_with_key(42i32, height);
                assert_eq!((*data).height(), height, "data node height={height}");
                assert!(!(*data).is_sentinel(), "data node must not be sentinel");
                assert_eq!(*(*data).key(), 42, "data node key");
                SkipTrieNode::dealloc_node(data);

                // Sentinel node: sentinel flag must be set
                let sentinel = SkipTrieNode::<i32>::alloc_sentinel(height);
                assert_eq!((*sentinel).height(), height, "sentinel height={height}");
                assert!((*sentinel).is_sentinel(), "sentinel must be sentinel");
                assert_eq!(*(*sentinel).key(), 0, "sentinel key is T::default()");
                SkipTrieNode::dealloc_node(sentinel);
            }
        }
    }

    #[test]
    fn test_pointer_at_initialized_null() {
        // All next pointers must be null after allocation.
        for height in [0, 3, 20] {
            unsafe {
                let node = SkipTrieNode::alloc_with_key(99i32, height);
                for level in 0..=height {
                    assert!(
                        (*node).get_next(level).is_null(),
                        "level {level} should be null after alloc (height={height})"
                    );
                }
                SkipTrieNode::dealloc_node(node);
            }
        }
    }

    #[test]
    fn test_pointer_write_read_roundtrip() {
        // Verify set_next_relaxed/get_next round-trip through the FAM pointer array.
        unsafe {
            let a = SkipTrieNode::alloc_with_key(1i32, 3);
            let b = SkipTrieNode::alloc_with_key(2i32, 0);

            // Link b at each level of a, then read back.
            for level in 0..=3 {
                (*a).set_next_relaxed(level, b);
                assert_eq!(
                    (*a).get_next(level),
                    b,
                    "level {level}: get_next should return the pointer set by set_next_relaxed"
                );
            }

            SkipTrieNode::dealloc_node(a);
            SkipTrieNode::dealloc_node(b);
        }
    }

    #[test]
    fn test_drop_correctness_arc() {
        // Verify dealloc_node drops the key properly by tracking Arc strong count.
        use std::sync::Arc;

        let arc = Arc::new(());
        let weak = Arc::downgrade(&arc);
        assert_eq!(weak.strong_count(), 1);

        unsafe {
            let node = SkipTrieNode::alloc_with_key(arc, 3);
            // Arc moved into node — strong count still 1
            assert_eq!(weak.strong_count(), 1);

            SkipTrieNode::dealloc_node(node);
            // Key dropped — strong count 0
            assert_eq!(weak.strong_count(), 0);
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn test_drop_correctness_sentinel_arc() {
        // Sentinel keys (T::default()) must also be dropped.
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        #[derive(Default, PartialEq, Eq, PartialOrd, Ord)]
        struct Counted(u8);
        impl Drop for Counted {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }

        DROP_COUNT.store(0, Ordering::Relaxed);
        unsafe {
            let sentinel = SkipTrieNode::<Counted>::alloc_sentinel(2);
            assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 0);

            SkipTrieNode::dealloc_node(sentinel);
            assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn test_key_bits_for_pair() {
        use crate::data_structures::pair::Pair;

        // BIT_WIDTH delegates to the key type
        assert_eq!(
            <Pair<usize, String> as KeyBits>::BIT_WIDTH,
            <usize as KeyBits>::BIT_WIDTH,
        );
        assert_eq!(
            <Pair<i32, String> as KeyBits>::BIT_WIDTH,
            <i32 as KeyBits>::BIT_WIDTH,
        );

        // to_bits() delegates to the key's to_bits()
        let p = Pair::new(42usize, "hello".to_string());
        assert_eq!(p.to_bits(), 42usize.to_bits());

        // Order preservation: a.key < b.key => a.to_bits() < b.to_bits()
        let pairs: Vec<Pair<usize, &str>> = (0..100).map(|i| Pair::new(i, "v")).collect();
        for w in pairs.windows(2) {
            assert!(w[0].to_bits() < w[1].to_bits(), "order not preserved");
        }

        // Order preservation for signed keys (sign-bit flip)
        let signed_pairs: Vec<Pair<i32, &str>> = (-50..50).map(|i| Pair::new(i, "v")).collect();
        for w in signed_pairs.windows(2) {
            assert!(
                w[0].to_bits() < w[1].to_bits(),
                "signed order not preserved"
            );
        }

        // Boundary values for signed keys (i32)
        assert!(Pair::new(i32::MIN, "").to_bits() < Pair::new(i32::MIN + 1, "").to_bits());
        assert!(Pair::new(i32::MAX - 1, "").to_bits() < Pair::new(i32::MAX, "").to_bits());
        assert!(Pair::new(-1i32, "").to_bits() < Pair::new(0i32, "").to_bits());

        // Boundary values for signed keys (i64)
        assert!(Pair::new(i64::MIN, "").to_bits() < Pair::new(i64::MIN + 1, "").to_bits());
        assert!(Pair::new(i64::MAX - 1, "").to_bits() < Pair::new(i64::MAX, "").to_bits());
        assert!(Pair::new(-1i64, "").to_bits() < Pair::new(0i64, "").to_bits());
    }
}
