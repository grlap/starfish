# starfish-crossbeam

Epoch-based memory-safe wrappers for lock-free concurrent data structures.

## Overview

`starfish-crossbeam` provides memory-safe wrappers that add epoch-based garbage collection to the lock-free data structures from `starfish-core`. It bridges the gap between unsafe low-level algorithms and safe, ergonomic APIs for production concurrent code.

## Features

- **Epoch-based memory reclamation** - Uses crossbeam-epoch for safe deferred destruction
- **Zero raw pointer exposure** - All unsafe operations encapsulated in wrappers
- **GuardedRef pattern** - References bundled with epoch guards prevent use-after-free
- **Generic over inner collections** - Works with any SortedCollection or HashMapCollection
- **Thread-safe iteration** - Iterators hold guards for their entire lifetime
- **Batch insert optimization** - Leverages NodePosition for O(1) amortized sorted inserts

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        USER CODE (Safe)                         │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│   EpochGuardedCollection<T, C>    EpochGuardedHashMap<K, V, C>  │
│   • Epoch guard on every op       • Epoch guard on every op     │
│   • Deferred node destruction     • Deferred node destruction   │
│   • GuardedRef returns            • GuardedRef returns          │
│                                                                 │
└──────────────┬────────────────────────────────┬─────────────────┘
               │ wraps                          │ wraps
               ▼                                ▼
┌─────────────────────────────────────────────────────────────────┐
│                      starfish-core (Unsafe)                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│   SortedList, SkipList            SplitOrderedHashMap           │
│   (lock-free sorted lists)        (lock-free hash table)        │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Quick Start

### Sorted Collections

```rust
use starfish_crossbeam::EpochGuardedCollection;
use starfish_core::data_structures::SkipList;

// Create a thread-safe sorted collection
let list: EpochGuardedCollection<i32, SkipList<i32>> =
    EpochGuardedCollection::default();

// Insert values (thread-safe)
list.insert(5);
list.insert(10);
list.insert(3);

// Check membership
assert!(list.contains(&5));

// Find with guarded reference
if let Some(val) = list.find(&10) {
    println!("Found: {}", *val);
}

// Iterate (holds epoch guard for entire iteration)
for item in list.iter() {
    println!("Item: {}", *item);
}

// Range iteration
for item in list.iter_from(&5) {
    println!("Item >= 5: {}", *item);
}

// Atomic update (key never "missing" during update)
list.update(10);

// Delete
list.delete(&5);
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
    println!("Alice's score: {}", *val);
}

// Check existence
assert!(map.contains(&"bob".to_string()));

// Remove and get value
let removed = map.remove(&"bob".to_string());
assert_eq!(removed, Some(200));

// Apply function without keeping reference
let doubled = map.find_and_apply(&"alice".to_string(), |_, v| v * 2);
```

### Concurrent Usage

```rust
use std::sync::Arc;
use std::thread;
use starfish_crossbeam::EpochGuardedCollection;
use starfish_core::data_structures::SkipList;

let list = Arc::new(EpochGuardedCollection::<i32, SkipList<i32>>::default());

let handles: Vec<_> = (0..8)
    .map(|t| {
        let list = Arc::clone(&list);
        thread::spawn(move || {
            for i in 0..1000 {
                list.insert(t * 1000 + i);
            }
        })
    })
    .collect();

for handle in handles {
    handle.join().unwrap();
}

println!("Total items: {}", list.len());
```

## Core Types

### EpochGuardedCollection<T, C>

Wraps any `SortedCollection<T>` with epoch-based memory safety.

```rust
pub struct EpochGuardedCollection<T, C>
where
    C: SortedCollection<T>,
    T: Eq + Ord,
```

**Key methods:**
- `insert(key: T) -> bool` - Insert a key
- `delete(key: &T) -> bool` - Delete without returning value
- `remove(key: &T) -> Option<T>` - Delete and return value
- `update(new_value: T) -> bool` - Atomic in-place update
- `find(key: &T) -> Option<GuardedRef<T>>` - Find with guarded reference
- `contains(key: &T) -> bool` - Check membership
- `iter() -> Iter` - Iterate all items
- `iter_from(start: &T) -> Iter` - Iterate from a key
- `insert_batch(iter: I) -> usize` - Optimized batch insert

### EpochGuardedHashMap<K, V, C>

Wraps any `HashMapCollection<K, V>` with epoch-based memory safety.

```rust
pub struct EpochGuardedHashMap<K, V, C>
where
    C: HashMapCollection<K, V>,
```

**Key methods:**
- `insert(key: K, value: V) -> bool` - Insert or update
- `remove(key: &K) -> Option<V>` - Remove and return value
- `get(key: &K) -> Option<GuardedRef<V>>` - Get with guarded reference
- `contains(key: &K) -> bool` - Check key exists
- `find_and_apply<F, R>(key: &K, f: F) -> Option<R>` - Apply function to value
- `len() -> usize` - Number of entries
- `is_empty() -> bool` - Check if empty

### GuardedRef<'g, T>

A reference bundled with an epoch guard to prevent use-after-free.

```rust
pub struct GuardedRef<'g, T> {
    _guard: Guard,
    reference: &'g T,
}
```

**Why it's needed:** Without GuardedRef, returning `&T` from `find()` would be unsafe because the epoch guard would drop before the reference could be used. GuardedRef ties the reference lifetime to the guard.

## Memory Reclamation

The crate uses crossbeam-epoch for memory reclamation:

1. **Epoch Guard Pinning** - Every operation pins an epoch guard
2. **Deferred Destruction** - Removed nodes are scheduled for later deletion
3. **Safe Reclamation** - Nodes freed only when no threads hold references

```
Thread 1: pin() → read node → unpin()
Thread 2: pin() → delete node → defer(node) → unpin()
          ↓
Epoch advances when all threads unpin
          ↓
Deferred nodes safely deallocated
```

## Performance

| Operation | Complexity | Notes |
|-----------|------------|-------|
| Insert | O(log n)* | Lock-free, one epoch guard |
| Delete | O(log n)* | Deferred reclamation |
| Find | O(log n)* | Wait-free reads |
| Iteration | O(n) | Single guard for entire iteration |
| Batch Insert | O(n)** | NodePosition optimization |

\* With SkipList backend; O(n) with SortedList
\** Amortized O(1) per item for sorted data

## Comparison with DeferredCollection

| Aspect | EpochGuardedCollection | DeferredCollection |
|--------|----------------------|-------------------|
| Memory Reclamation | Epoch-based (concurrent) | Deferred until drop |
| Thread Safety | Fully concurrent | Single-threaded |
| Use Case | Production systems | Testing only |
| Overhead | Epoch guard per op | Minimal |

## Dependencies

```toml
[dependencies]
crossbeam-epoch = "0.9"
starfish-core = "0.1"
```

## Related Crates

| Crate | Description |
|-------|-------------|
| `starfish-core` | Lock-free data structure algorithms |
| `starfish-reactor` | Async I/O reactor |
| `starfish-tls` | TLS/SSL support |
| `starfish-storage` | Storage layer |

## License

See the repository root for license information.
