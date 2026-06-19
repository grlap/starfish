# starfish-epoch

Custom epoch-based memory reclamation integrated with the Starfish reactor runtime.

## Overview

`starfish-epoch` provides a custom epoch-based reclamation system designed to work with the `Coordinator` and `Reactor` from `starfish-reactor`. Unlike `starfish-crossbeam` which uses the crossbeam-epoch library, this crate implements its own epoch tracking with per-reactor counters and cooperative epoch advancement.

## Modules

| Module | Description |
|--------|-------------|
| `epoch` | Core epoch tracking: `Epoch` and `EpochGuard` for pin/unpin lifecycle |
| `epoch_alloc` | Global allocator wrapper using MiMalloc for high-frequency alloc/dealloc patterns |

## Key Types & Traits

| Type | Description |
|------|-------------|
| `Epoch` | Global epoch tracker — `pin()`, `bump_epoch()`, `local_epoch()`, `deferred_delete()` |
| `EpochGuard` | RAII guard that pins the current epoch during data structure traversal (`!Send`) |
| `EpochAlloc` | Global allocator delegating to MiMalloc |

## Usage Example

```rust,ignore
use starfish_epoch::epoch::Epoch;

// Pin the current epoch before accessing lock-free data structures
let guard = Epoch::pin();

// ... traverse data structures safely ...

// Unpin when done (guard's Drop calls unpin automatically)
drop(guard);

// Schedule a previously-dropped object for deferred deallocation
let ptr = Box::into_raw(boxed_node);
unsafe {
    core::ptr::drop_in_place(ptr);
    Epoch::deferred_delete(ptr);
}
```

## Design Notes

**3-state epoch model.** Three intrusive deferred-deallocation lists per reactor, indexed by `epoch % 3`. Garbage tagged at epoch E is safe to reclaim once the global epoch reaches E+2 — advancing from E+1 to E+2 requires all reactors to have observed E+1, guaranteeing no reactor still holds references from epoch E.

**Incremental reclamation.** On `pin()`, if the global epoch has advanced past the reactor's `local_epoch`, all stale garbage bags are drained before the reactor proceeds. No separate GC pass is needed.

**Intrusive lists.** Deferred allocations reuse the freed object's own memory for list bookkeeping (next pointer + size u32 + align u32 = 16 bytes). `EpochAlloc` enforces a minimum allocation of 16 bytes, so all types fit with zero extra allocations.

**`deferred_delete` semantics.** Defers memory deallocation only — the caller must drop the value before scheduling. Same as crossbeam's `defer_unchecked` + `dealloc`.

**Cooperative integration.** The epoch system integrates with the Starfish runtime via the `CooperativeInitializer` trait. Each `Reactor` thread gets its own `EpochCounter`, and the global epoch advances only when all reactors have observed the current epoch.

## Dependencies

```toml
[dependencies]
mimalloc = "0.1"
starfish-reactor = "0.1"
```

## Related Crates

| Crate | Description |
|-------|-------------|
| `starfish-crossbeam` | Alternative epoch reclamation using crossbeam-epoch |
| `starfish-core` | Lock-free data structures that use epoch-based reclamation |
| `starfish-reactor` | Reactor runtime providing Coordinator integration |
