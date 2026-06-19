# starfish-crossbeam

Epoch-based memory reclamation for starfish lock-free collections using crossbeam-epoch.

## Overview

`starfish-crossbeam` provides `EpochGuard`, an implementation of the `Guard` trait from `starfish-core` that uses crossbeam-epoch for safe deferred destruction of lock-free data structure nodes. Collections parameterized with `EpochGuard` automatically get epoch-based memory reclamation.

## Modules

| Module | Description |
|--------|-------------|
| `epoch_guard` | `EpochGuard` and `EpochRef` — Guard trait implementation using crossbeam-epoch |
| `bin/profile_skiplist` | Profiling binary for sorted collections (SkipList, SkipTrie) |
| `bin/profile_hashmap` | Profiling binary for hash map collections |

## Key Types & Traits

| Type | Description |
|------|-------------|
| `EpochGuard` | Zero-sized `Guard` implementation that defers node destruction via crossbeam-epoch |
| `EpochRef<'a, T>` | A reference bundled with an epoch guard to prevent use-after-free |

## Usage Example

```rust,ignore
use starfish_core::data_structures::{SortedList, SortedCollection};
use starfish_crossbeam::EpochGuard;

// Create a lock-free sorted list with epoch-based reclamation
let list: SortedList<i32, EpochGuard> = SortedList::new();

// All operations use epoch-based reclamation
list.insert(42);
list.insert(17);

if let Some(val) = list.find(&42) {
    println!("Found: {}", *val);
}

list.delete(&42);
```

## Design Notes

`EpochGuard` is a zero-sized type — it carries no per-instance state. Instead, it schedules destruction through the global crossbeam-epoch collector. When a node is removed from a collection, the `Guard::retire` implementation calls `epoch::pin()` and defers deallocation until all threads have advanced past the current epoch.

This design means:
- **No per-guard overhead** — pinning happens at operation time, not guard creation
- **Thread-safe reclamation** — nodes are freed only when no thread can hold a reference
- **Transparent integration** — any `SortedCollection<T, EpochGuard>` or `SplitOrderedHashMap<K, V, EpochGuard>` automatically gets safe memory reclamation

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
| `starfish-epoch` | Custom epoch-based memory reclamation (alternative to crossbeam) |
| `starfish-reactor` | Async I/O reactor |
