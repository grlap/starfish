# Data Structures Hierarchy

This document describes the architecture and hierarchy of the concurrent data structures in this crate.

## Overview

The design follows a layered architecture that separates low-level algorithms from memory safety concerns.
Both sorted collections and hash maps follow the same pattern:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           USER-FACING API (Safe)                        │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   ┌─────────────────────────────┐     ┌─────────────────────────────┐   │
│   │   EpochGuardedCollection    │     │    EpochGuardedHashMap      │   │
│   │        (struct)             │     │        (struct)             │   │
│   │   [starfish-crossbeam]      │     │   [starfish-crossbeam]      │   │
│   │                             │     │                             │   │
│   │  • insert(T)                │     │  • insert(K, V)             │   │
│   │  • delete(&T)               │     │  • remove(&K) -> V          │   │
│   │  • find(&T) -> GuardRef     │     │  • get(&K) -> GuardRef      │   │
│   │  • contains(&T)             │     │  • contains(&K)             │   │
│   │  • update(T)                │     │  • find_and_apply(...)      │   │
│   │  • Epoch-based reclamation  │     │  • Epoch-based reclamation  │   │
│   └─────────────┬───────────────┘     └─────────────┬───────────────┘   │
│                 │                                   │                   │
└─────────────────┼───────────────────────────────────┼───────────────────┘
                  │ wraps                             │ wraps
                  ▼                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                      LOW-LEVEL TRAITS                                   │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   ┌─────────────────────────┐         ┌─────────────────────────┐       │
│   │    SortedCollection     │         │   HashMapCollection     │       │
│   │        (trait)          │         │        (trait)          │       │
│   │                         │         │                         │       │
│   │  • insert_from_internal │         │  • insert_internal      │       │
│   │  • remove_from_internal │         │  • remove_internal      │       │
│   │  • find_from_internal   │         │  • find_internal        │       │
│   │  • update_internal      │         │  • apply_on_internal    │       │
│   │                         │         │                         │       │
│   │  Returns: *mut Node     │         │  Returns: *mut Node     │       │
│   │  (raw pointers!)        │         │  (raw pointers!)        │       │
│   └───────────┬─────────────┘         └───────────┬─────────────┘       │
│               │                                   │                     │
└───────────────┼───────────────────────────────────┼─────────────────────┘
                │ implemented by                    │ implemented by
                ▼                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                      DATA STRUCTURE IMPLEMENTATIONS                     │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   ┌───────────────┐  ┌───────────────┐  ┌───────────────────────────┐   │
│   │  SortedList   │  │   SkipList    │  │   SplitOrderedHashMap     │   │
│   │   (struct)    │  │   (struct)    │  │        (struct)           │   │
│   │               │  │               │  │                           │   │
│   │  O(n) search  │  │  O(log n)     │  │  Lock-free hash table     │   │
│   │  Lock-free    │  │  Lock-free    │  │  Uses SortedList inside   │   │
│   └───────────────┘  └───────────────┘  └───────────────────────────┘   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### Key Design Principles

1. **Users interact with EpochGuardedCollection/HashMap** - no raw pointers exposed
2. **Memory-safe wrappers** convert unsafe internal operations to safe APIs
3. **GuardedRef** bundles epoch guards with references to prevent use-after-free
4. **Two wrapper options**:
   - `EpochGuardedCollection/HashMap` - production use with proper reclamation
   - `DeferredCollection/HashMap` - testing only, defers cleanup until drop

### Quick Usage

```rust
// PRODUCTION: Sorted collection with epoch-based reclamation
let list = EpochGuardedCollection::new(SkipList::new());
list.insert(42);

// PRODUCTION: Hash map with epoch-based reclamation
let map = EpochGuardedHashMap::new(SplitOrderedHashMap::new());
map.insert("key", "value");

// TESTING: Deferred cleanup (for predictable test behavior)
let list = DeferredCollection::new(SortedList::new());
let map = DeferredHashMap::new(SplitOrderedHashMap::new());
```

## Trait Hierarchy

### SortedCollection (Low-Level)

The `SortedCollection` trait defines the low-level algorithm interface. It exposes raw pointers and is not meant for direct use by application code.

```rust
pub trait SortedCollection<T: Eq + Ord> {
    type Node: CollectionNode<T>;
    type NodePosition: NodePosition<T, Node = Self::Node>;

    // Core operations (return NodePosition for batch optimization)
    fn insert_from_internal(&self, key: T, position: Option<&Self::NodePosition>) -> Option<Self::NodePosition>;
    fn remove_from_internal(&self, position: Option<&Self::NodePosition>, key: &T) -> Option<Self::NodePosition>;
    fn find_from_internal(&self, position: Option<&Self::NodePosition>, key: &T, exact: bool) -> Option<Self::NodePosition>;
    fn update_internal(&self, position: Option<&Self::NodePosition>, new_value: T)
        -> Option<(*mut Self::Node, Self::NodePosition)>;

    // Iteration support
    fn first_node_internal(&self) -> Option<*mut Self::Node>;
    fn next_node_internal(&self, node: *mut Self::Node) -> Option<*mut Self::Node>;

    // Node operations
    fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>;

    // Batch insert (uses NodePosition for O(1) amortized inserts on sorted data)
    fn insert_batch<I>(&self, iter: I) -> usize
    where
        I: OrderedIterator<Item = T>;
}
```

The `NodePosition` trait stores predecessors at ALL levels, enabling O(1) amortized batch inserts
for sorted data. This optimization is similar to RocksDB's "splice" pattern for MemTable inserts.

### Atomic Update Operation (UPDATE_MARK)

The `update_internal` method provides atomic in-place updates using marked pointers.

**Key Guarantees:**
- `find_from_internal` only returns nodes available in the current epoch (unmarked nodes)
- Any marked nodes are always physically unlinked before `remove_internal` returns
- Readers never see "half-updated" state - either old value or new value

**Algorithm (SortedList - Forward Insertion):**
1. Find the node `curr` with the target key
2. Create `new_node` with the new value
3. Set `new_node.next = curr.next` (successor)
4. **LINEARIZATION POINT**: CAS `curr.next` to `(new_node | UPDATE_MARK)`
5. CAS `pred.next` from `curr` to `new_node` (snip curr out)
6. Return `(old_node_ptr, new_position)`

**Interaction with DELETE:**
- Both DELETE_MARK and UPDATE_MARK use the low bits of the next pointer
- `find_from_internal` checks `is_any_marked()` to skip both DELETE and UPDATE marked nodes
- `remove_from_internal` checks `is_any_marked()` for ownership - first thread to mark the node wins
- Marked nodes are fully unlinked before the function returns

**Implementation Status:**
| Implementation | update_internal |
|----------------|-----------------|
| SortedList | ✅ Atomic UPDATE_MARK (forward insertion) |
| SkipList | ✅ Atomic UPDATE_MARK (forward insertion) |

### Insert-or-Update Pattern (SplitOrderedHashMap)

The `SplitOrderedHashMap::insert` method uses an optimistic insert-then-update pattern
that combines fast-path insertion with atomic updates:

```rust
// Pseudocode for insert(key, value)
fn insert(&self, key: K, value: V) -> Option<V> {
    let new_entry = Entry::new(key, value);

    // Step 1: Try optimistic insert (fast path for new keys)
    if self.list.insert_from(&sentinel_pos, new_entry.clone()) {
        self.size.fetch_add(1, Ordering::Relaxed);
        return None;  // Key was new
    }

    // Step 2: Key exists - atomic update (never "missing")
    match self.list.update_internal(&sentinel_pos, new_entry.clone()) {
        Some((old_node, _)) => {
            // Atomically swapped old → new
            // Clone value since other threads may still be reading old_node
            return Some(unsafe { (*old_node).key().clone() });
        }
        None => {
            // Key deleted between insert and update attempts
            // Step 3: Retry insert
            if self.list.insert_from(&sentinel_pos, new_entry) {
                self.size.fetch_add(1, Ordering::Relaxed);
            }
            None
        }
    }
}
```

**Why this pattern?**

1. **Optimistic Fast Path**: Most inserts are for new keys. Attempting insert first
   avoids the overhead of searching for existing keys when they don't exist.

2. **Atomic Updates**: When a key exists, `update_internal` atomically replaces the
   old value. Readers always see either the old or new value, never a "missing" key.

3. **Race Handling**: If another thread deletes the key between the failed insert
   and the update attempt, `update_internal` returns `None`, and we retry the insert.

**Comparison with DELETE + INSERT:**

| Approach | During Update | Atomicity |
|----------|--------------|-----------|
| DELETE + INSERT | Key temporarily missing | ❌ Two-phase, can observe gap |
| update_internal | Key always present | ✅ Single linearization point |

**Example Race Scenario:**
```
Thread A: insert(K, V1)         Thread B: get(K)
─────────────────────────────────────────────────
1. insert_from fails (K exists)
2.                              get(K) → sees V0
3. update_internal marks old node
   (LINEARIZATION POINT)
4.                              get(K) → sees V1
5. pred.next CASed to new node
6. return Some(V0)
```

At step 4, Thread B sees V1 even before step 5 because the UPDATE-marked old node's
`next` pointer leads to the new node.

## Data Structure Implementations

### SortedList

Lock-free sorted linked list using Harris's algorithm with marked pointers for deletion.

```
Forward:   Head ──► [1] ──► [3] ──► [5] ──► [7] ──► NULL
```

**Characteristics:**
- O(n) search, insert, delete
- Good for small collections or when used with split-ordered hashing
- **Batch insert optimization**: 6-7x faster for sorted batch inserts (transforms O(n²) → O(n))

### SkipList

Lock-free skip list with multiple levels for O(log n) operations.

```
Level 3:  Head ─────────────────────────► [5] ─────────────────────► Tail
Level 2:  Head ────────► [3] ────────────► [5] ──────► [7] ────────► Tail
Level 1:  Head ──► [1] ──► [3] ──► [4] ──► [5] ──► [6] ──► [7] ──► Tail
Level 0:  Head ──► [1] ──► [2] ──► [3] ──► [4] ──► [5] ──► [6] ──► [7] ──► Tail
```

**Characteristics:**
- O(log n) average for search, insert, delete
- Probabilistic balancing (16 levels, p=0.5)
- Forward-only traversal
- **Batch insert optimization**: ~5-15% faster for sorted batch inserts
- **Best-effort higher levels**: Level 0 is critical; levels 1+ break on CAS failure (elegant design)

### SkipList (with Level-Based Recovery)

Extended skip list using preds[level+1] for efficient recovery from marked nodes.

```
Level 3:  Head ─────────────────────────────────────► 30 ─────────────────► NULL
Level 2:  Head ──────────► 10 ─────────────────────► 30 ─────────────────► NULL
Level 1:  Head ──────────► 10 ──────────► 20 ──────► 30 ─────────────────► NULL
Level 0:  Head ──────────► 10 ──────────► 20 ──────► 30 ──────────► 40 ──► NULL

Recovery uses preds[level+1] when predecessor at level L is marked:
  preds = [20, 20, 10, HEAD]
  If preds[0]=20 is marked, recover from preds[1]=20, then preds[2]=10, then HEAD
```

**Variable Height Nodes:**

Unlike fixed-height skip lists, each node has a random height H.
A node only has next[0..H] pointers (no backward pointers - recovery uses preds array).
When finding predecessors to delete node X:
- preds[L] only needs links at level L, not at all levels
- Example: preds = [20, 20, 10, HEAD] - node 20 (height=2) is pred at levels 0,1

**Characteristics:**
- O(log n) operations (insert, delete, find, update)
- 16 levels with p=0.5 probability
- Level-based recovery using preds[level+1] (memory efficient, no prev pointers)
- **Batch insert optimization**: ~10-17% faster for sorted batch inserts
- **Best-effort higher levels**: Level 0 is critical; levels 1+ break on CAS failure
- **Atomic UPDATE**: Creates replacement node with same height as original (preserves balance)

### Level-by-Level DELETE Algorithm (SkipList)

**Delete processes levels TOP-DOWN: mark → unlink at each level.**

```rust
// For each level from (height-1) down to 1:
//   1. Mark this level (CAS to add DEL flag)
//   2. Unlink at this level (use preds[level+1] for recovery if needed)
//
// For level 0 (ownership):
//   1. Mark level 0 (ownership CAS - only ONE thread wins)
//   2. If we own: unlink at level 0, return node
//   3. If we don't own: return None (another DELETE or UPDATE owns it)
```

**Key Points:**
- Higher levels are marked FIRST, level 0 is marked LAST
- Ownership is determined by which thread successfully marks level 0 (with DELETE_MARK or UPDATE_MARK)
- Any thread can help unlink higher levels (cooperative)
- Only the owner unlinks level 0 and returns the node
- Recovery from marked predecessors uses preds[level+1]

**Recovery via preds[level+1]:**
When a predecessor at level L is marked, use the predecessor from level L+1:

```rust
// recover_pred: find a valid predecessor using higher levels
fn recover_pred(&self, level: usize, preds: &[*mut SkipNode<T>]) -> *mut SkipNode<T> {
    for check_level in (level + 1)..preds.len() {
        let pred = preds[check_level];
        if pred is valid and has height > level {
            return pred;
        }
    }
    return self.head; // fallback
}
```

### Recovery Patterns by Data Structure

**SortedList: Start-node Restart Recovery**
- Restarts traversal from provided start_node or HEAD
- When CAS fails during snipping, restart from start_node
- Compatible with split-ordered hashmap bucket sentinels as start nodes

**SkipList (with Level-Based Recovery): preds[level+1]-based Recovery**
- Memory efficient (no backward pointers needed)
- Recovery uses predecessors from higher levels in the preds array
- Expected O(1) - typically only need to go up one level
- Worst case O(log n) if many levels have stale preds

### Batch Insert Optimization

The `NodePosition` trait enables dramatic performance improvements for sorted batch inserts
across all data structures. The improvement varies by structure due to algorithmic differences.

#### SortedList Batch Insert (O(n²) → O(n))

For SortedList, batch insert provides **6-7x speedup** (or more for larger sizes):

**The Problem:**
Normal SortedList insertion is O(n) because we traverse from HEAD to find the position.
Inserting n sorted elements naively requires O(1) + O(2) + ... + O(n) = O(n²) traversal.

**The Solution:**
NodePosition stores the predecessor pointer. For sorted batch inserts, each new element
goes immediately after the previous one. The predecessor from the last insert IS the
predecessor for the next insert - requiring O(1) traversal instead of O(k).

```
Insert sorted: [1, 2, 3, 4, 5]

Without batch (O(n²)):
  Insert 1: traverse from HEAD → find position → O(1)
  Insert 2: traverse from HEAD → skip 1 → O(2)
  Insert 3: traverse from HEAD → skip 1,2 → O(3)
  Insert 4: traverse from HEAD → skip 1,2,3 → O(4)
  Insert 5: traverse from HEAD → skip 1,2,3,4 → O(5)
  Total: 1+2+3+4+5 = 15 = O(n²)

With batch (O(n)):
  Insert 1: traverse from HEAD → find position → O(1), save pos
  Insert 2: start from node 1 → O(1), save pos
  Insert 3: start from node 2 → O(1), save pos
  Insert 4: start from node 3 → O(1), save pos
  Insert 5: start from node 4 → O(1), save pos
  Total: 1+1+1+1+1 = 5 = O(n)
```

#### SkipList Batch Insert (~10-15% improvement)

When inserting sorted data using `insert_batch`, both skip list implementations use a
"start hint" optimization that improves performance by ~10-15%:

**The Problem:**
Normal skip list insertion is O(log n) because we traverse from HEAD at the top level
down to level 0, taking ~log(n) steps. For batch inserts of sorted data, each insert
starts from HEAD even though the previous insertion point is nearby.

**The Solution:**
Pass the previously inserted node as a "start hint". For levels below the hint's height,
start traversal from the hint instead of HEAD.

```
Skip List with start_hint optimization:

Inserting sorted batch: [10, 20, 30, 40, ...]

Insert 10: Normal traversal from HEAD (no hint)
           Node 10 has height 2

Insert 20: Use node 10 as hint
           - For level 0: start from node 10 (instead of HEAD)
           - For level 1: start from node 10 (instead of HEAD)
           - For levels >= 2: still traverse from HEAD (hint doesn't reach)
           Saves ~2 levels of traversal!

Insert 30: Use node 20 as hint (height 1)
           - For level 0: start from node 20
           - For levels >= 1: traverse from HEAD
           Saves ~1 level of traversal
```

**Complexity Analysis:**
- Skip list node heights follow geometric distribution (p=0.5):
  - ~50% of nodes have height 1
  - ~25% have height 2
  - ~12.5% have height 3, etc.
- Average node height ≈ 2 levels
- With max_level = 16, we save ~2/16 ≈ 12.5% of traversal
- Still **O(log n)** but with better constant factors

Note: True O(log d) "finger search" where d is distance from hint would require
a more complex algorithm that determines the optimal starting level.

**Hint Validity Checks:**
The hint is only used if:
1. Hint is not null
2. Hint is not marked (not deleted)
3. Hint's key < target key (hint must be BEFORE the insertion point)

If any check fails, we fall back to normal O(log n) traversal from HEAD.

**Batch Insert Benchmark Results:**
| Implementation | Size | Batch Time | Individual Time | Improvement |
|----------------|------|------------|-----------------|-------------|
| SortedList | 1K | 14µs | 110µs | **-87%** |
| SortedList | 10K | 150µs | 10.8ms | **-99%** |
| SortedList | 100K | 1.7ms | 1.1s | **-99.8%** |
| SkipList | 1K | 109µs | 141µs | -23% |
| SkipList | 10K | 1.1ms | 1.3ms | -15% |
| SkipList | 100K | 12ms | 12ms | ~0% |

Note: SortedList batch insert transforms O(n²) → O(n), while SkipList stays O(n log n) with better constants.

### SkipList vs Crossbeam SkipMap Comparison

**Concurrent Insert (1000 ops/thread, 10K elements):**
| Threads | SkipList | Crossbeam SkipMap | Winner |
|---------|----------|-------------------|--------|
| 1 | 1.15 ms | 0.96 ms | Crossbeam (1.2x) |
| 2 | 1.47 ms | 1.56 ms | **SkipList (1.06x)** |
| 4 | 2.07 ms | 2.71 ms | **SkipList (1.31x)** |
| 8 | 3.88 ms | 6.51 ms | **SkipList (1.68x)** |
| 12 | 5.82 ms | 10.11 ms | **SkipList (1.74x)** |
| 16 | 7.20 ms | 13.59 ms | **SkipList (1.89x)** |

**Mixed Workload (50% insert, 25% delete, 25% lookup):**
| Threads | SkipList | Crossbeam SkipMap | Winner |
|---------|----------|-------------------|--------|
| 1 | 1.56 ms | 1.19 ms | Crossbeam (1.31x) |
| 2 | 2.61 ms | 2.33 ms | Crossbeam (1.12x) |
| 4 | 4.35 ms | 4.16 ms | Crossbeam (1.05x) |
| 8 | 8.40 ms | 8.89 ms | **SkipList (1.06x)** |
| 12 | 12.08 ms | 15.06 ms | **SkipList (1.25x)** |
| 16 | 20.36 ms | 21.98 ms | **SkipList (1.08x)** |

**High Contention (same key range, 100 keys):**
| Threads | SkipList | Crossbeam SkipMap | Winner |
|---------|----------|-------------------|--------|
| 1 | 0.88 ms | 1.33 ms | **SkipList (1.51x)** |
| 2 | 1.76 ms | 4.44 ms | **SkipList (2.52x)** |
| 4 | 1.88 ms | 6.64 ms | **SkipList (3.53x)** |
| 8 | 5.33 ms | 13.81 ms | **SkipList (2.59x)** |
| 12 | 3.66 ms | 18.40 ms | **SkipList (5.03x)** |
| 16 | 4.62 ms | 24.11 ms | **SkipList (5.22x)** |

**Sequential Batch Insert (single-thread):**
| Size | SkipList | Crossbeam SkipMap | Winner |
|------|----------|-------------------|--------|
| 1K | 96 µs | 70 µs | Crossbeam (1.37x) |
| 10K | 991 µs | 812 µs | Crossbeam (1.22x) |
| 100K | 10.33 ms | 8.86 ms | Crossbeam (1.17x) |
| 200K | 21.84 ms | 19.55 ms | Crossbeam (1.12x) |
| 500K | 56.84 ms | 49.12 ms | Crossbeam (1.16x) |

**Summary:**
- **Low contention/single-threaded**: Crossbeam wins by ~15-30%
- **High concurrency (4+ threads)**: SkipList wins by 1.3-1.9x on inserts
- **High contention**: SkipList wins dramatically (2.5-5x faster)
- SkipList excels under concurrency due to finer-grained synchronization
- Crossbeam is more efficient for single-threaded workloads with optimized memory layout

## Memory Safety Wrappers

### DeferredCollection

Defers node destruction until the collection is dropped. Useful for testing where you want predictable cleanup timing.

```
┌─────────────────────────────────────────┐
│          DeferredCollection             │
│  ┌───────────────┐  ┌────────────────┐  │
│  │    inner      │  │ deferred_nodes │  │
│  │ (collection)  │  │    (Vec)       │  │
│  └───────────────┘  └────────────────┘  │
│         │                   │           │
│         ▼                   ▼           │
│    SortedList         [ptr, ptr, ...]   │
│    or SkipList        (freed on drop)   │
└─────────────────────────────────────────┘
```

### EpochGuardedCollection

Uses crossbeam-epoch for safe memory reclamation in concurrent scenarios.

```
┌─────────────────────────────────────────────────────────┐
│              EpochGuardedCollection                     │
│  ┌───────────────┐                                      │
│  │    inner      │                                      │
│  │ (collection)  │                                      │
│  └───────────────┘                                      │
│         │                                               │
│         ▼                                               │
│  ┌─────────────────────────────────────────────────┐    │
│  │              crossbeam-epoch                    │    │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────┐          │    │
│  │  │ Epoch 0 │  │ Epoch 1 │  │ Epoch 2 │  ...     │    │
│  │  └─────────┘  └─────────┘  └─────────┘          │    │
│  │       │                                         │    │
│  │       ▼                                         │    │
│  │  Deferred garbage collected when safe           │    │
│  └─────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

**Epoch Guard Requirements:**

Every operation that traverses the collection MUST pin an epoch guard BEFORE accessing nodes:

```rust
fn insert(&self, key: T) -> bool {
    let _guard = epoch::pin();  // CRITICAL: Pin before traversing!
    self.inner.insert_from_internal(key, None).is_some()
}
```

Without epoch guards, concurrent deletions can free nodes while another thread traverses them → use-after-free.

## GuardedRef Pattern

References to values are wrapped in `GuardedRef` types that bundle the reference with its memory protection guard:

```
┌────────────────────────────────────────┐
│             GuardedRef<T>              │
│  ┌──────────────┐  ┌────────────────┐  │
│  │    guard     │  │   data: &T     │  │
│  │ (epoch pin)  │  │  (reference)   │  │
│  └──────────────┘  └────────────────┘  │
│         │                  │           │
│         ▼                  ▼           │
│  Prevents reclamation   Safe access    │
│  while held             to value       │
└────────────────────────────────────────┘
```

## File Organization

```
data_structures/
├── mod.rs                         # Module exports and re-exports
├── README.md                      # This file
├── iterable_collection.rs         # IterableCollection traits
├── ordered_iterator.rs            # OrderedIterator for batch operations
│
├── internal/                      # Internal implementation details
│   ├── mod.rs
│   ├── marked_ptr.rs              # MarkedPtr (2-bit) for lock-free deletion
│   ├── marked_ptr_3bit.rs         # MarkedPtr (3-bit) for DELETE/UPDATE/DEL_NEXT
│   └── sorted_collection.rs       # SortedCollection trait
│
├── sorted/                        # Sorted collection implementations
│   ├── mod.rs
│   ├── sorted_list.rs             # SortedList (O(n) linked list)
│   ├── skip_list.rs               # SkipList (O(log n))
│   └── treap.rs                   # Treap implementation (sequential)
│
├── hash/                          # Hash-based collections
│   ├── mod.rs
│   ├── split_ordered_hash_map.rs  # Split-ordered hash map
│   └── hash_map_collection.rs     # HashMapCollection trait (low-level)
│
└── trie/                          # Trie implementations
    ├── mod.rs
    └── skip_trie.rs               # SkipTrie implementation
```

**In starfish-crossbeam:**
```
starfish-crossbeam/src/
├── lib.rs                         # EpochGuardedCollection, EpochGuardedHashMap
└── epoch_guard.rs                 # EpochGuard implementation
```

## Usage Example

### Sorted Collections

```rust
use starfish_crossbeam::EpochGuardedCollection;
use starfish_core::data_structures::SkipList;

// Create a thread-safe sorted collection
let collection: EpochGuardedCollection<i32, SkipList<i32>> =
    EpochGuardedCollection::default();

// Insert values
collection.insert(5);
collection.insert(3);
collection.insert(7);

// Safe iteration
for item in collection.iter() {
    println!("{}", *item);  // 3, 5, 7
}

// Range iteration
for item in collection.iter_from(&5) {
    println!("{}", *item);  // 5, 7
}

// Find with guarded reference
if let Some(val) = collection.find(&5) {
    println!("Found: {}", *val);
}

// Atomic update (replace value, readers never see "missing")
collection.update(5);  // Replace 5 with 5 atomically
```

### Hash Maps

```rust
use starfish_crossbeam::EpochGuardedHashMap;
use starfish_core::data_structures::SplitOrderedHashMap;

// Create a thread-safe hash map
let map: EpochGuardedHashMap<String, i32, SplitOrderedHashMap<String, i32>> =
    EpochGuardedHashMap::default();

// Insert key-value pairs
map.insert("alice".to_string(), 100);
map.insert("bob".to_string(), 200);

// Get with guarded reference
if let Some(val) = map.get(&"alice".to_string()) {
    println!("Alice's score: {}", *val);  // 100
}

// Check existence
assert!(map.contains(&"bob".to_string()));

// Remove and get value
let removed = map.remove(&"bob".to_string());
assert_eq!(removed, Some(200));

// Apply function to value
let doubled = map.find_and_apply(&"alice".to_string(), |_, v| v * 2);
assert_eq!(doubled, Some(200));
```

## Design Rationale

1. **Separation of Concerns**: Low-level algorithms (`SortedCollection`) are separate from memory-safe wrappers (`EpochGuardedCollection`), allowing different reclamation strategies.

2. **Encapsulation**: Internal node pointers are never exposed to users. The `inner()` method is `pub(crate)` only.

3. **Extensibility**: New data structures implement `SortedCollection` trait, new safety wrappers wrap them.

4. **Iterator Safety**: Iterators hold guards that prevent memory reclamation during iteration.
