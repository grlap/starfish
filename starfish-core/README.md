# starfish-core

Core library providing high-performance, lock-free concurrent data structures and synchronization primitives for the Starfish ecosystem.

## Overview

`starfish-core` is the foundational crate of the Starfish project, implementing lock-free concurrent data structures designed for extreme concurrency scenarios. The library provides both low-level algorithm implementations and trait-based abstractions that enable pluggable memory reclamation strategies.

## Features

- **Lock-free data structures** - Concurrent sorted lists, skip lists, and hash maps
- **Atomic update operation** - UPDATE_MARK-based in-place updates without "missing key" windows
- **Batch insert optimization** - NodePosition-based O(1) amortized inserts for sorted data (6-7x faster for SortedList)
- **Trait-based design** - Pluggable memory reclamation strategies (epoch-based, hazard pointers, reference counting)
- **Synchronization primitives** - CountdownEvent and future utilities
- **Comprehensive test framework** - Reusable concurrent test suite for collection implementations

## Architecture

The library follows a layered architecture separating algorithm implementation from memory safety:

```
User Code
   ↓ uses
EpochGuardedCollection         ← Epoch-based memory safety (starfish-crossbeam)
   ↓ wraps
SortedCollection               ← Low-level algorithm trait
   ↓ implemented by
SortedList, SkipList, etc.     ← Actual data structures
```

## Data Structures

### Sorted Collections

| Structure | Complexity | Batch Insert | Description |
|-----------|------------|--------------|-------------|
| `SortedList` | O(n) | 6-7x faster | Harris's lock-free linked list |
| `SkipList` | O(log n) | ~10% faster | Probabilistic multi-level skip list (16 levels, p=0.5) |

### Hash Maps

| Structure | Complexity | Description |
|-----------|------------|-------------|
| `SplitOrderedHashMap` | O(1) avg | Lock-free hash map with split-ordered list |

### Other Structures

| Structure | Description |
|-----------|-------------|
| `Treap` | Randomized binary search tree (sequential) |
| `DeferredCollection` | Test wrapper with deferred node destruction |

## Core Traits

### `SortedCollection<T>`

Low-level trait for sorted collection algorithms:

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

    // Batch insert (uses NodePosition for O(1) amortized inserts on sorted data)
    fn insert_batch<I>(&self, iter: I) -> usize
    where
        I: OrderedIterator<Item = T>;
}
```

The `NodePosition` trait stores predecessors at ALL levels, enabling O(1) amortized batch inserts for sorted data (similar to RocksDB's "splice" pattern).

The `update_internal` method provides atomic in-place updates using UPDATE_MARK. The key is never "missing" during an update - readers always see either the old or new value.

### `HashMapCollection<K, V>`

Low-level trait for hash map algorithms:

```rust
pub trait HashMapCollection<K: Hash + Eq, V> {
    type Node: HashMapNode<K, V>;

    fn insert_internal(&self, key: K, value: V) -> Option<*mut Self::Node>;
    fn remove_internal(&self, key: &K) -> Option<*mut Self::Node>;
    fn find_internal(&self, key: &K) -> Option<*mut Self::Node>;
    fn contains(&self, key: &K) -> bool;
    fn get(&self, key: &K) -> Option<V> where V: Clone;
}
```

## Synchronization Primitives

### CountdownEvent

A synchronization primitive similar to Java's `CountDownLatch`:

```rust
use starfish_core::preemptive_synchronization::CountdownEvent;

let event = CountdownEvent::new(3);

// In worker threads
event.signal();  // Decrements count

// In main thread
event.wait();    // Blocks until count reaches 0
```

### FutureExtension

Utility trait for synchronously evaluating futures:

```rust
use starfish_core::preemptive_synchronization::FutureExtension;

let result = async { 42 }.unwrap_result();
assert_eq!(result, 42);
```

## Usage Example

```rust
use starfish_core::data_structures::sorted_list::SortedList;
use starfish_core::data_structures::sorted_collection::SortedCollection;

// Create a concurrent sorted list
let list = SortedList::<i32>::new();

// Insert elements (thread-safe)
list.insert(5);
list.insert(10);
list.insert(3);

// Check membership
assert!(list.contains(&5));
assert!(!list.contains(&99));

// Find and apply function
let doubled = list.find_and_apply(&5, |x| x * 2);
assert_eq!(doubled, Some(10));
```

For production use with proper memory reclamation, use `starfish-crossbeam`:

```rust
use starfish_crossbeam::EpochGuardedCollection;
use starfish_core::data_structures::sorted_list::SortedList;

let list = EpochGuardedCollection::new(SortedList::<i32>::new());

list.insert(5);
if let Some(val) = list.find(&5) {
    println!("Found: {}", *val);
}

// Atomic node replacement - key is never "missing" during the update
list.update(5);  // Atomically replaces the node containing 5

list.delete(&5);
```

## Testing

The crate includes a comprehensive test framework for sorted collections:

```rust
use starfish_core::common_tests::sorted_collection_core_tests;

// Run standard test suite on your implementation
sorted_collection_core_tests::test_basic_operations::<MyCollection>();
sorted_collection_core_tests::test_concurrent_operations::<MyCollection>();
sorted_collection_core_tests::test_concurrent_mixed_operations::<MyCollection>();
```

Enable the test framework feature:

```toml
[features]
default = ["sorted_collection_core_tests"]
```

## Benchmarks

Run the concurrent benchmarks:

```bash
cargo bench --bench concurrent_benchmark
```

## Related Crates

| Crate | Description |
|-------|-------------|
| `starfish-epoch` | Epoch-based memory reclamation |
| `starfish-crossbeam` | Crossbeam-epoch integration |
| `starfish-reactor` | Async I/O reactor |
| `starfish-storage` | Storage layer with epoch-guarded collections |

## Memory Safety

The low-level data structures (`SortedList`, `SkipList`, etc.) intentionally leak memory - deleted nodes are unlinked but not freed. This design enables lock-free deletion without use-after-free bugs.

For automatic memory reclamation, wrap collections with:
- `EpochGuardedCollection` (recommended) - Uses epoch-based reclamation
- `HazardPointerCollection` - Uses hazard pointers
- `DeferredCollection` - Defers destruction until drop (testing only)

## Algorithm Invariants

### Pointer Marking System

The library uses two mark bits in pointer LSBs for lock-free coordination:

| Mark | Bit | Purpose |
|------|-----|---------|
| `DELETE_MARK` | 0 | Node is logically deleted, next pointer points to successor |
| `UPDATE_MARK` | 1 | Node is being updated, next pointer points to replacement node |

### Critical Traversal Invariant

**All traversal code MUST use `is_any_marked()` (not `is_marked()`) when checking if a node needs unlinking.**

```rust
// CORRECT - handles both DELETE and UPDATE marks
if next_marked.is_any_marked() {
    // Unlink the marked node...
}

// WRONG - only handles DELETE mark, causes livelock with UPDATE
if next_marked.is_marked() {  // DON'T DO THIS
    // ...
}
```

When a node is marked (either DELETE or UPDATE), traversing threads help by physically unlinking it:
- **DELETE-marked**: Snip to `node.next` (the successor)
- **UPDATE-marked**: Snip to `node.next` (the replacement node B')

Failing to handle UPDATE-marked nodes causes livelock under concurrent updates.

### Epoch Guard Requirements

**Every operation that traverses the collection MUST pin an epoch guard BEFORE accessing any nodes.**

This is critical because:
1. INSERT/UPDATE/DELETE all traverse the list via `find_position`
2. Concurrent operations may free nodes during traversal
3. Without a guard, freed nodes may be dereferenced → **use-after-free**

```rust
// In EpochGuardedCollection
fn insert(&self, key: T) -> bool {
    // MUST pin epoch guard! Insert traverses the list via find_position,
    // which accesses existing nodes that could be freed by concurrent deletes.
    let _guard = epoch::pin();
    self.inner.insert_from_internal(key, None).is_some()
}
```

Operations requiring epoch guards:
- `insert`, `insert_from`, `insert_batch` - traverse to find insertion point
- `delete`, `remove` - traverse to find and unlink target
- `update` - traverse to find, replace, and unlink
- `find`, `contains`, `find_and_apply` - traverse to locate target
- `is_empty`, `iter`, `iter_from` - access list nodes

### Node Removal Requirements for Epoch Guard

**Old nodes returned from `update_internal` and `remove_internal` MUST be physically unlinked from the list before the function returns.**

This is critical for epoch-based memory reclamation:

1. `EpochGuardedCollection` schedules returned nodes for deferred destruction
2. Epoch guard assumes the node is unreachable from the list
3. If the node remains linked, concurrent readers may access it after destruction → **use-after-free**

```rust
// update_internal returns the OLD node for destruction
fn update_internal(...) -> Option<(*mut Self::Node, Self::NodePosition)> {
    // ... mark old_node with UPDATE_MARK ...
    // ... unlink old_node from ALL levels ...
    return Some((old_node, position));  // old_node MUST be unlinked
}

// remove_internal returns the REMOVED node for destruction
fn remove_internal(...) -> Option<Self::NodePosition> {
    // ... mark node with DELETE_MARK ...
    // ... unlink node from ALL levels ...
    // NodePosition.current() returns the removed node
}
```

### INSERT/DELETE Coordination (Skip Lists with 0|DEL)

Skip list nodes are inserted bottom-up (level 0 first, then higher levels). This creates a race:
INSERT may be linking higher levels while DELETE is trying to remove the same node.

**The Problem:**
```
INSERT: links level 0, then level 1, then level 2...
DELETE: finds node, marks levels, unlinks...
```

If DELETE marks levels while INSERT is still linking, there's a race condition.

**Solution: 0|DEL (null pointer with DELETE mark)**

DELETE claims unlinked levels by setting `node.next[level] = 0|DEL`. INSERT checks for this signal.

### DELETE Algorithm (SkipList)

```
1. FIND POSITION
   - find_position returns preds[] and succs[] at all levels

2. CLAIM UNLINKED LEVELS WITH 0|DEL (height-1 → 1, top-down)
   - For each level from height-1 down to 1:
     - If node.next[level] == 0: CAS to 0|DEL
     - If node.next[level] != 0: level is LINKED
   - Purpose: Signal INSERT to stop

3. MARK AND UNLINK HIGHER LINKED LEVELS (height-1 → 1)
   - Skip levels with 0|DEL (not linked)
   - Mark: CAS node.next[level] = succ → succ|DEL
   - Unlink: CAS pred.next[level] = node → succ

4. MARK LEVEL 0 (linearization point - ownership)
   - CAS: node.next[0] = succ → succ|DEL
   - If already marked, return None (another thread owns it)

5. UNLINK LEVEL 0
   - CAS: pred.next[0] = node → succ
   - Return node for deferred destruction
```

### INSERT Algorithm (SkipList)

```
1. FIND POSITION
   - find_position returns preds[] and succs[] at all levels

2. CREATE NEW NODE
   - node.next[0] = succs[0]
   - Higher levels stay NULL (set by insert_at_level)

3. LINK LEVEL 0 (linearization point)
   - CAS: preds[0].next[0] = succs[0] → new_node
   - If fails, deallocate and retry

4. LINK HIGHER LEVELS (1 → height-1, bottom-up)
   For each level, insert_at_level does:

   a. CHECK FOR DELETE SIGNAL
      - If node.next[height-1] is 0|DEL → STOP

   b. SET NODE'S NEXT POINTER (atomically!)
      - CAS: node.next[level] = 0 → succ
      - If fails with 0|DEL → STOP (DELETE claimed it)

   c. HANDLE PRED ISSUES
      - If pred is marked → recover from higher level or restart

   d. LINK NODE
      - CAS: pred.next[level] = succ → new_node
```

**Key Coordination:**
- DELETE claims `height-1` FIRST (top-down)
- INSERT checks `height-1` BEFORE linking any level (bottom-up)
- Once DELETE claims `height-1` with `0|DEL`, INSERT sees it and stops

**Critical: Use CAS, not Store**

When INSERT updates `node.next[level]`, it MUST use CAS:
```rust
// WRONG - can overwrite DELETE's mark!
node.set_next(level, new_succ);

// CORRECT - respects concurrent marking
let result = node.cas_next(level, old_succ, new_succ);
if result.is_err() && is_marked(result) {
    return false;  // DELETE marked it, stop
}
```

### Update Algorithm (UPDATE_MARK)

The atomic update algorithm ensures keys are never "missing" during updates:

```
Before:  pred → B → succ
                ↓
Step 1:  pred → B(UPD→B') → succ    (CAS B.next[0] = B'|UPDATE_MARK)
                ↓
Step 2:  Mark B.next[1..height] = B'|UPDATE_MARK (all levels point to B'!)
                ↓
Step 3:  pred → B' → succ              (Unlink B from all levels)
```

- **Linearization point**: CAS that sets `B.next[0] = B' | UPDATE_MARK`
- Readers seeing UPDATE_MARK follow the pointer to find B' (the new value)
- Traversing threads help unlink B when they encounter the UPDATE_MARK

**Critical Invariants**:
1. The new node B' MUST have the SAME HEIGHT as B (preserves skip list balance)
2. At ALL levels, `B.next[level] = B' | UPDATE_MARK` (not succ!)
   - If we marked with succ, find_at_level would snip to succ, bypassing B'
   - This would break the skip list structure and cause hangs/cycles

### Recovery from Marked Predecessors

When a predecessor is marked during traversal:
- **SortedList**: Restart traversal from provided start_node or HEAD
- **SkipList**: Use preds[level+1] to recover from higher level, or restart from HEAD

## License

See the repository root for license information.
