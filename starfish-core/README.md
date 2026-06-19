# starfish-core

Core library providing high-performance, lock-free concurrent data structures and synchronization primitives for the Starfish ecosystem.

## Overview

`starfish-core` is the foundational crate of the Starfish project, implementing lock-free concurrent data structures designed for extreme concurrency scenarios. The library provides both low-level algorithm implementations and trait-based abstractions that enable pluggable memory reclamation strategies.

## Features

- **Lock-free data structures** - Concurrent sorted lists, skip lists, and hash maps
- **Atomic value-CAS update** - `SkipList` maps update via an atomic value-pointer swap (`MapEntry`), never replacing the node — epoch-safe, no "missing key" window
- **Batch insert optimization** - NodePosition-based O(1) amortized inserts for sorted data (6-7x faster for SortedList)
- **Trait-based design** - Pluggable memory reclamation strategies (epoch-based, hazard pointers, reference counting)
- **Synchronization primitives** - CountdownEvent and future utilities
- **Comprehensive test framework** - Reusable concurrent test suite for collection implementations

## Architecture

The library follows a layered architecture separating algorithm implementation from memory safety:

```
User Code
   ↓ uses
SkipList<T, EpochGuard>        ← Epoch-based memory safety (starfish-crossbeam)
   ↓ EpochGuard implements Guard
SortedCollection               ← Low-level algorithm trait
   ↓ implemented by
SortedList, SkipList, etc.     ← Actual data structures
```

## Data Structures

### Sorted Collections

| Structure | Complexity | Batch Insert | Description |
|-----------|------------|--------------|-------------|
| `SortedList` | O(n) | 6-7x faster | Harris's lock-free linked list |
| `SkipList` | O(log n) | ~10% faster | Probabilistic multi-level skip list (32 levels, p=0.5) |

### Hash Maps

| Structure | Complexity | Description |
|-----------|------------|-------------|
| `SplitOrderedHashMap` | O(1) avg | Lock-free hash map with split-ordered list |

### Trie-Based Collections

| Structure | Complexity | Description |
|-----------|------------|-------------|
| `SkipTrie` | O(log log u) | Concurrent sorted map combining x-fast trie prefix hashing with skip list buckets |
| `YFastTrie` | O(log log u) | Concurrent key-value map with sorted-bucket partitioning |

### Other Structures

| Structure | Description |
|-----------|-------------|
| `Treap` | Randomized binary search tree (sequential) |

## Modules

| Module | Description |
|--------|-------------|
| `data_structures` | Lock-free sorted collections, hash maps, tries, and internal primitives |
| `data_structures::sorted` | Lock-free sorted collections (`SkipList`, `SortedList`, `Treap`) |
| `data_structures::hash` | Hash-based collections (`SplitOrderedHashMap`, `MapCollection` trait) |
| `data_structures::trie` | Trie-based structures (`SkipTrie`, `YFastTrie`) |
| `data_structures::internal` | Internal implementation details (marked pointers, collection traits) |
| `guard` | `Guard` trait for pluggable memory reclamation strategies |
| `preemptive_synchronization` | Blocking synchronization (`CountdownEvent`, `FutureExtension`) |
| `common_tests` | Reusable test suites and benchmark utilities |

## Core Traits

### `SortedCollectionInternal<T>` (crate-private)

Low-level trait for sorted collection algorithms. All methods are `unsafe fn`
requiring the caller to hold a pinned read guard (`Guard::pin()`):

```rust
pub trait SortedCollectionInternal<T: Eq + Ord> {
    type Node: CollectionNode<T>;
    type NodePosition: NodePosition<T, Node = Self::Node>;

    // Core operations — all unsafe, require pinned guard
    unsafe fn insert_from_internal(&self, key: T, position: Option<&Self::NodePosition>) -> Option<Self::NodePosition>;
    unsafe fn remove_from_internal(&self, position: Option<&Self::NodePosition>, key: &T) -> Option<Self::NodePosition>;
    unsafe fn find_from_internal(&self, position: Option<&Self::NodePosition>, key: &T, exact: bool) -> Option<Self::NodePosition>;

    // Node-replacement UPDATE — on the core trait since all three sorted
    // collections implement it (the old separate `NodeReplaceUpdate` trait
    // existed only while SkipList could not). The replaced node is retired
    // INTERNALLY by every implementor.
    unsafe fn update_internal(&self, position: Option<&Self::NodePosition>, new_value: T)
        -> Option<Self::NodePosition>;
}
```

The safe public API is `SortedCollection<T>`, which pins the guard internally and delegates to the `unsafe` methods. It also provides `insert_batch` which uses `NodePosition` for O(1) amortized inserts on sorted data (similar to RocksDB's "splice" pattern).

Node-replacement UPDATE is `SortedCollectionInternal::update_internal`, implemented by all three sorted collections (it briefly lived on a separate `NodeReplaceUpdate` trait while `SkipList` could not support node replacement; folded back 2026-06-10). A *set* has no separable value, so the public `SortedCollection::update` is a **presence check** (replacing an element with an `Eq` element is a no-op); node-replacement matters for the **maps**. `SkipList` offers both map backends, selected by payload type: `SkipList<MapEntry<K,V>>` updates via a single value-pointer CAS (no `UPDATE_MARK`, value behind one indirection), while `SkipList<Pair<K,V>>` updates by **forward-splice node replacement** — one CAS marks the old carrier and links its same-key replacement, so the key is never absent, the value stays inline in the node, and the old node's teardown reuses the deferred physical-delete handshake. Node-replacement UPDATE is validated **epoch-safe** for all three implementors (EpochGuard + ASan stress; single-level, trie, and multi-level-tower): the old node is provably unreachable before it is retired, and forward iteration skips a node's same-key UPDATE replacement so it never yields a key twice. Retirement is uniform and **internal**: every implementor unlinks AND retires the replaced node itself (`update_internal` returns only the new position) — callers never retire anything.

### `MapCollectionInternal<K, V>` / `MapCollection<K, V>`

Low-level trait (`MapCollectionInternal`) for key-value map algorithms; the safe `MapCollection` supertrait adds the guard-pinning convenience methods:

```rust
pub trait MapCollectionInternal<K: Eq, V> {
    type Guard: Guard;
    type Node: MapNode<K, V>;

    fn guard(&self) -> &Self::Guard;
    fn insert_internal(&self, key: K, value: V) -> Option<*mut Self::Node>;
    fn remove_internal(&self, key: &K) -> Option<*mut Self::Node>;
    fn find_internal(&self, key: &K) -> Option<*mut Self::Node>;
    fn update_internal(&self, key: K, value: V) -> bool; // replace value, retire old internally
    fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>;
    fn len_internal(&self) -> usize;
}

// The safe `MapCollection<K, V>: MapCollectionInternal<K, V>` supertrait adds the
// guard-pinning convenience methods: insert, remove, contains, get, update, find_and_apply.
```

For maps, **UPDATE is a value-CAS, not node replacement.** The `SkipList` map payload is `MapEntry<K, V>` (key inline + value behind an `AtomicPtr<V>`); `update` CASes the value pointer, then retires the **old value** (not the node) through the guard. The node is never replaced, so the key is never "missing" and there is no old-node reclamation hazard — epoch-safe by construction. The value pointer also carries the entry's **logical presence** (null = tombstone): `remove` linearizes by *claiming* the value (null swap) before physically deleting the node, `update`'s CAS fails on the tombstone, and reads treat null as absent — update↔remove races serialize on one atomic, so a remove always returns the then-current value (no lost updates; `MapCollection::remove` is overridden to return the claimed value). `SkipList<MapEntry<K,V>>` additionally exposes `get_ref(&K) -> Option<GuardedRef<V>>`, a zero-copy guard-protected value read (the map analog of `SortedCollection::find`). Node-replacement maps (`SortedList`/`SkipTrie`/`SplitOrderedHashMap`) retire the old node inside their `update_internal`. Set-API mutation (`SortedCollection::insert/delete`) on a map-typed `SkipList` is **out of contract** — use the `MapCollection` API exclusively.

### Additional Public Traits & Types

| Type / Trait | Module | Description |
|--------------|--------|-------------|
| `Guard` | `guard` | Trait for pluggable memory reclamation (epoch, deferred, hazard) |
| `IterableCollection<T>` | `iterable_collection` | Safe iteration over collections with guarded references |
| `IterableSortedCollection<T>` | `iterable_collection` | Extended iteration with `iter_from` range queries |
| `OrderedIterator` | `ordered_iterator` | Marker trait for iterators yielding elements in ascending order |
| `Ordered<I>` | `ordered_iterator` | Runtime-verified ordered iterator wrapper for batch operations |
| `Pair<K, V>` | `pair` | Key-value wrapper (compares by key only); map payload for `SortedList` / `SkipTrie` (value stored inline) |
| `MapEntry<K, V>` | `map_entry` | Map payload for `SkipList` — key inline + value behind `AtomicPtr<V>`, enabling epoch-safe value-CAS UPDATE |

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
use starfish_crossbeam::EpochGuard;
use starfish_core::data_structures::sorted_list::SortedList;
use starfish_core::data_structures::sorted_collection::SortedCollection;

let list: SortedList<i32, EpochGuard> = SortedList::new();

list.insert(5);
if let Some(val) = list.find(&5) {
    println!("Found: {}", *val);
}

// For a set, update(5) confirms 5 is present (replacing an element with an
// equal element is a no-op). Key->value maps update via value-CAS — the map
// payload is `MapEntry`, and `SkipList::get_ref` reads a value without cloning.
list.update(5);

list.delete(&5);
```

## Testing

The crate includes reusable test frameworks for both `SortedCollection` and `MapCollection`:

```rust
use starfish_core::common_tests::sorted_collection_core_tests;
use starfish_core::common_tests::map_collection_core_tests;

// Run standard SortedCollection test suite
sorted_collection_core_tests::test_basic_operations::<MyCollection>();
sorted_collection_core_tests::test_concurrent_operations::<MyCollection>();

// Run MapCollection test suite
map_collection_core_tests::test_insert_contains_get::<MyMap>();
map_collection_core_tests::test_concurrent_insert_get_remove::<MyMap>();
```

Enable the test framework features:

```toml
[features]
default = ["sorted_collection_core_tests", "map_collection_core_tests"]
```

The optional `bench_utils` feature provides shared benchmark helpers (e.g., dynamic thread-count generation) used across `starfish-crossbeam` benchmarks.

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

For automatic memory reclamation, parameterize collections with a `Guard` type:
- `SkipList<T, EpochGuard>` (recommended) - Uses epoch-based reclamation via crossbeam
- `SortedList<T, EpochGuard>` - Same, for linked lists
- `SortedList<T, DeferredGuard>` - Defers destruction until guard drops (testing only)

## Algorithm Invariants

### Pointer Marking System

The library uses two mark bits in pointer LSBs for lock-free coordination:

| Mark | Bit | Purpose |
|------|-----|---------|
| `DELETE_MARK` | 0 | Node is logically deleted, next pointer points to successor |
| `UPDATE_MARK` | 1 | Node is being updated, next pointer points to replacement node |

### Critical Traversal Invariant

**All traversal code MUST use `is_any_marked()` (not `is_delete_marked()`) when checking if a node needs unlinking.**

```rust
// CORRECT - handles both DELETE and UPDATE marks
if next_marked.is_any_marked() {
    // Unlink the marked node...
}

// WRONG - only handles DELETE mark, causes livelock with UPDATE
if next_marked.is_delete_marked() {  // DON'T DO THIS
    // ...
}
```

When a node is marked (either DELETE or UPDATE), traversing threads help by physically unlinking it:
- **DELETE-marked**: Snip to `node.next` (the successor)
- **UPDATE-marked**: Snip to `node.next` (the replacement node B')

Failing to handle UPDATE-marked nodes causes livelock under concurrent updates.

> Scope note: since the value-CAS redesign, `UPDATE_MARK` is produced **only** by the
> node-replacement UPDATE of `SortedList`/`SkipTrie`. `SkipList` maps update via value-CAS and
> never set `UPDATE_MARK`; SkipList traversal still tolerates the mark defensively (dead but
> harmless), so the `is_any_marked()` rule above still holds for all structures.

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
    // SAFETY: guard pinned above protects against concurrent reclamation.
    unsafe { self.inner.insert_from_internal(key, None) }.is_some()
}
```

Operations requiring epoch guards:
- `insert`, `insert_from`, `insert_batch` - traverse to find insertion point
- `delete`, `remove` - traverse to find and unlink target
- `update` - SkipList map: traverse to find, then value-CAS (no unlink); SortedList/SkipTrie: traverse, replace, unlink; SkipList set: presence check
- `find`, `contains`, `find_and_apply`, `get_ref` - traverse to locate target
- `is_empty`, `iter`, `iter_from` - access list nodes

### Node Removal Requirements for Epoch Guard

**A replaced or removed node MUST be physically unlinked before it is retired.** `update_internal` enforces this internally for all implementors (the old node is unlinked, then `defer_destroy`'d by the collection itself); `remove_internal` likewise must unlink before the node is retired (SortedList/SkipTrie: caller retires the returned node; SkipList: retirement is internal to the physical-phase winner). The `SkipList<MapEntry>` map `update` is a value-CAS that retires the old *value*, not a node, so it has nothing to unlink.

This is critical for epoch-based memory reclamation:

1. `EpochGuardedCollection` schedules returned nodes for deferred destruction
2. Epoch guard assumes the node is unreachable from the list
3. If the node remains linked, concurrent readers may access it after destruction → **use-after-free**

```rust
// Node-replacement update_internal (SortedList/SkipTrie) returns the OLD node
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

### Update Algorithm

**SkipList maps — value-CAS (epoch-safe).** The `MapEntry<K,V>` payload holds the value behind an
`AtomicPtr<V>`. UPDATE swaps the value pointer with a single CAS and retires the **old value**
through the guard. The node is never touched, so the key is never "missing" and there is no
old-node reclamation hazard:

```
update(K, v'):  p = node.value.swap(into_raw(v'))   // single CAS — linearization point
                guard.defer_destroy(p)               // retire the OLD value (refcount-free)
```

`get_ref(&K)` reads a value without cloning: it hands the value pointer to `Guard::make_ref`,
returning a `GuardedRef<V>` whose own pinned guard keeps the value alive while held.

**SortedList / SkipTrie — node-replacement (Mark-and-Append).** These still replace the node: a
same-key node `B'` is appended after the matched node `B`, predecessors are relinked to `B'`, and
`B` is unlinked and deferred:

```
Before:  pred → B → succ
Step 1:  pred → B ──╳U──► B' → succ   (CAS B.next[0]: succ → B'|UPDATE_MARK; linearization point)
Step 2:  relink predecessors → B'
Step 3:  pred → B' → succ              (unlink B; B orphaned, deferred free)
```

- Readers seeing `UPDATE_MARK` follow the pointer to `B'` (the new value); traversing threads help
  unlink `B`. This is why all traversal uses `is_any_marked()` (see above).
- The replacement `B'` is allocated at the same height as `B` to preserve structure.

> Node-replacement UPDATE was **removed from SkipList** specifically: its *helping* variant could
> `defer_destroy` the old node while a concurrent traversal still reached it (use-after-free,
> reproduced with EpochGuard + ASan). `SortedList`/`SkipTrie` use non-helping mark-then-insert with
> a guaranteed unlink-before-retire and are **validated epoch-safe** (EpochGuard + ASan,
> `starfish-crossbeam/tests/{sorted_list,skip_trie}_map_epoch.rs`); their iterators skip a node's
> same-key UPDATE replacement, so iteration is duplicate-free. They keep node-replacement; value-CAS
> is preferred only for its single-CAS UPDATE and for letting `UPDATE_MARK` eventually be removed.

### Recovery from Marked Predecessors

When a predecessor is marked during traversal:
- **SortedList**: Restart traversal from provided start_node or HEAD
- **SkipList**: Use preds[level+1] to recover from higher level, or restart from HEAD

## License

See the repository root for license information.
