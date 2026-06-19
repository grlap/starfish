# Epoch Domains

Independent epoch namespaces with per-domain reclamation cadence, token-based pinning,
and repin support.

**Status:** NOT IMPLEMENTED
**Crate:** starfish-epoch
**Key files:** src/epoch.rs, src/lib.rs

---

## Problem

Today `starfish-epoch` has a **single global epoch** shared by all reactors. Advancing from
epoch E to E+1 requires *every* reactor to have observed E. This creates two problems:

1. **Cross-subsystem blocking** — a reactor doing a long memtable scan pins epoch E. Every
   other subsystem (hot indexes, WAL cleanup, buffer pool eviction) is blocked from
   reclaiming garbage until that scan completes, even though their garbage is completely
   unrelated.

2. **One cadence fits none** — a hot B-tree index wants aggressive reclamation (low memory
   overhead). A memtable wants *no* reclamation (bulk-free the whole thing on flush). A
   single global epoch can't serve both.

```
                         Global Epoch (today)
                   ┌──────────────────────────┐
                   │  current_epoch_index: E  │
                   │                          │
  Reactor 0 ──────►│  counters[0] ──► EpochCounter { local: E, bags[3] }
  Reactor 1 ──────►│  counters[1] ──► EpochCounter { local: E, bags[3] }
  Reactor 2 ──────►│  counters[2] ──► EpochCounter { local: E-1, bags[3] }  ← lagging
                    └──────────────────────────┘
                     bump_epoch() blocked: min_local < global
                     ALL reclamation stalled across ALL subsystems
```

## Design: Epoch Domains

An **EpochDomain** is an independent epoch namespace. Each domain has its own epoch
counter, per-reactor counters, and garbage bags. Domains advance independently — one
domain's lagging reactor doesn't block another domain's reclamation.

```
  Domain A (hot index)          Domain B (memtable)
  ┌──────────────────┐          ┌──────────────────┐
  │ epoch: 42        │          │ epoch: 3         │
  │ cadence: 10ms    │          │ cadence: Flush   │
  │                  │          │                  │
  │ R0: local=42     │          │ R0: local=3      │
  │ R1: local=42     │          │ R1: local=3      │
  │ R2: local=41     │          │ R2: (not joined) │
  └──────────────────┘          └──────────────────┘
  ▲ advancing freely            ▲ never advances — bulk-free on drop
```

Domains are **dynamic**: created at runtime, destroyed when no longer needed. A memtable
creates a domain at birth and drops it on flush. A long-lived index creates a domain once
and keeps it for the process lifetime.

### EpochDomain

```rust
/// An independent epoch namespace.
///
/// Each domain has its own epoch counter and per-reactor state.
/// Domains are created dynamically and can be destroyed (bulk-frees all garbage).
pub struct EpochDomain {
    id: DomainId,
    current_epoch_index: AtomicU32,
    cadence: Cadence,
    /// Per-reactor counters. Slot count = max reactor count at creation time.
    /// Null slots = reactor not participating in this domain.
    counters: Box<[AtomicPtr<DomainCounter>]>,
}
```

`DomainId` is a unique identifier (likely `u32`) assigned by the domain registry.

### DomainCounter (per-reactor, per-domain)

```rust
/// Per-reactor state within a single domain.
///
/// Same 3-state model as today's EpochCounter, but scoped to one domain.
struct DomainCounter {
    local_epoch: AtomicU32,
    pin_count: u32,
    generations: EpochGenerations,   // reuse existing 3-bag implementation
}
```

Each reactor gets a `DomainCounter` when it **joins** a domain (not at domain creation).
Reactors that never touch a domain have no overhead for it.

### Relationship to existing Epoch

The existing `Epoch` struct becomes a thin wrapper: one pre-created `EpochDomain` (the
"default domain") plus the domain registry. Existing call sites (`Epoch::pin()`,
`Epoch::bump_epoch()`) continue to work against the default domain.

```rust
pub struct Epoch {
    default_domain: EpochDomain,
    registry: DomainRegistry,
}

impl Epoch {
    /// Pin the default domain (backwards compatible).
    pub fn pin() -> EpochToken { Self::instance().default_domain.pin() }

    /// Bump the default domain (backwards compatible).
    pub fn bump_epoch() -> bool { Self::instance().default_domain.bump() }
}
```

---

## Domain Registry

Domains are registered and discovered through a lock-free registry on the `Epoch` struct.

```rust
struct DomainRegistry {
    /// Active domains, indexed by DomainId.
    /// Uses a lock-free data structure (slab or Vec<AtomicPtr<EpochDomain>>).
    domains: Vec<AtomicPtr<EpochDomain>>,
    next_id: AtomicU32,
}

impl Epoch {
    /// Create a new domain with the given cadence policy.
    /// Returns a handle that owns the domain's lifetime.
    pub fn create_domain(cadence: Cadence) -> DomainHandle {
        // Allocate DomainId, Box::leak an EpochDomain, store in registry.
        // Returns DomainHandle (RAII — drop destroys the domain).
    }
}
```

### DomainHandle (ownership)

```rust
/// Owns an EpochDomain's lifetime.
///
/// When dropped, waits for all pins to release, reclaims all garbage bags,
/// and removes the domain from the registry.
pub struct DomainHandle {
    domain: *const EpochDomain,
}

impl Drop for DomainHandle {
    fn drop(&mut self) {
        // 1. Assert no active pins (pin_count == 0 on all reactors)
        // 2. Reclaim all 3 garbage bags on all reactor counters
        // 3. Dealloc all DomainCounters
        // 4. Remove from registry
        // 5. Dealloc EpochDomain
    }
}
```

### Joining a domain

A reactor must **join** a domain before pinning it. Joining allocates a `DomainCounter`
for that reactor in the domain's counter array.

```rust
impl EpochDomain {
    /// Join this reactor to the domain. Allocates a DomainCounter.
    /// Idempotent — joining twice returns the existing counter.
    pub fn join(&self) -> &mut DomainCounter {
        let idx = Reactor::local_instance().reactor_index();
        let slot = &self.counters[idx];
        let ptr = slot.load(Ordering::Acquire);
        if !ptr.is_null() {
            return unsafe { &mut *ptr };
        }
        // Allocate, store with Release.
        let counter = Box::leak(Box::new(DomainCounter::new()));
        slot.store(counter, Ordering::Release);
        counter
    }
}
```

Joining is lazy — happens on first `pin()` for this domain on this reactor.

---

## Cadence Policy

Each domain is created with a cadence that controls epoch advancement.

```rust
/// How a domain's epoch advances.
pub enum Cadence {
    /// Application calls `domain.bump()` explicitly. (Current behavior.)
    Manual,

    /// Reactor event loop bumps every `interval` duration.
    /// Registered as a timer on each reactor that has joined the domain.
    Periodic { interval: Duration },

    /// Bump after `threshold` deferred deletions accumulate on this reactor.
    /// Checked inline during `deferred_delete`.
    Threshold { count: u32 },

    /// Never advance. All garbage is bulk-freed when the domain is dropped.
    /// Ideal for memtables: write-heavy, short-lived, flush = drop.
    Flush,
}
```

### Cadence integration points

| Cadence | Who bumps | When | Integration point |
|---------|-----------|------|-------------------|
| Manual | Application code | Explicit `domain.bump()` | Same as today |
| Periodic | Reactor event loop | Timer fires | `init_reactor` registers a repeating timer |
| Threshold | `deferred_delete` inline | Every N deferrals | Counter in `DomainCounter` |
| Flush | Nobody | Never — bulk-free on `DomainHandle::drop()` | Skip bump entirely |

### Threshold detail

```rust
impl DomainCounter {
    fn defer(&mut self, epoch: u32, ptr: *mut u8, layout: Layout) {
        self.generations.defer(epoch, ptr, layout);
        self.defer_count += 1;
        // Check threshold cadence.
        if self.defer_count >= self.threshold {
            self.defer_count = 0;
            // Attempt domain bump from this reactor.
            self.domain().try_bump();
        }
    }
}
```

### Periodic detail

On `join()`, if the domain has `Cadence::Periodic`, register a repeating timer on the
current reactor. The timer callback calls `domain.try_bump()`. On leave/drop, cancel
the timer.

---

## EpochToken

The `EpochToken` replaces today's `EpochGuard` as the RAII pin handle. It carries a
reference to the domain it's pinned in and supports repin.

```rust
/// RAII token that keeps a domain epoch pinned.
///
/// - `!Send + !Sync` — drop calls thread-local unpin.
/// - Carries the domain reference for repin and deferred_delete.
/// - Replaces EpochGuard (EpochGuard becomes a type alias for backwards compat).
pub struct EpochToken {
    domain: *const EpochDomain,
    epoch_index: u32,
    _not_send: PhantomData<*const ()>,
}
```

### Acquiring a token

```rust
impl EpochDomain {
    /// Pin this domain on the current reactor. Returns a token.
    pub fn pin(&self) -> EpochToken {
        let counter = self.join();  // lazy join
        // Same first-pin logic as today's EpochCounter::pin():
        // if pin_count == 0, observe global epoch, reclaim stale bags.
        counter.pin(self)
    }
}
```

### Token API

```rust
impl EpochToken {
    /// The epoch this token is observing.
    pub fn epoch_index(&self) -> u32 { self.epoch_index }

    /// The domain this token belongs to.
    pub fn domain(&self) -> &EpochDomain {
        unsafe { &*self.domain }
    }

    /// Schedule a deferred deallocation within this token's domain.
    ///
    /// # Safety
    /// Same preconditions as Epoch::deferred_delete, plus:
    /// - The allocation must belong to this domain (not another domain's garbage bags).
    pub unsafe fn defer_delete<T>(&self, object: *mut T) {
        let counter = DomainCounter::local_instance(self.domain);
        let layout = Layout::new::<T>();
        counter.generations.defer(self.epoch_index, object as *mut u8, layout);
    }

    /// Repin: re-observe the domain's global epoch.
    ///
    /// After repin, the token observes a newer (or same) epoch. Garbage between
    /// the old and new epoch may be reclaimed. **All raw pointers obtained before
    /// repin are invalidated.**
    ///
    /// Returns the previous epoch index (for diagnostics/logging).
    pub fn repin(&mut self) -> u32 {
        let prev = self.epoch_index;
        let counter = unsafe { DomainCounter::local_instance(self.domain) };
        let global = unsafe { &*self.domain }
            .current_epoch_index
            .load(Ordering::Acquire);

        if global != self.epoch_index {
            // Reclaim stale garbage bags between old and new epoch.
            if counter.pin_count == 1 {
                // Outermost pin — safe to advance local_epoch.
                counter.generations.reclaim(self.epoch_index, global);
                counter.local_epoch.store(global, Ordering::Release);
            }
            self.epoch_index = global;
        }
        prev
    }
}

impl Drop for EpochToken {
    fn drop(&mut self) {
        let counter = unsafe { DomainCounter::local_instance(self.domain) };
        counter.unpin();
    }
}
```

### Backwards compatibility

```rust
/// Backwards-compatible alias. Existing code using Epoch::pin() gets an EpochToken
/// pinned to the default domain.
pub type EpochGuard = EpochToken;
```

---

## Repin

Repin is a lightweight operation: re-read the domain's global epoch and update the
token's `epoch_index`. If this is the outermost pin (`pin_count == 1`), also advance
`local_epoch` and reclaim stale garbage — exactly what `pin()` does on first entry,
but performed while already pinned.

### Safety contract

After `repin()`:
- **All raw pointers obtained before repin are potentially invalid.** Freed nodes from
  the old epoch may have been reclaimed during the repin's `generations.reclaim()` call.
- The caller must re-navigate from a safe starting point (e.g., collection head, or
  re-seek by key).
- Position types (`SkipNodePosition`, `ListNodePosition`) must not be reused across a
  repin boundary. They are already move-only (`!Copy + !Clone`), which helps, but the
  caller must still ensure no stale position is used after repin.

### Why repin matters

Without repin, a long-running scan holds its epoch for the entire duration. Other
reactors can bump the global epoch, but *this* reactor's `local_epoch` won't advance
until it unpins. That blocks `bump_epoch()` for the entire domain once `min_local_epoch`
falls behind.

With repin, the scan periodically re-observes the global epoch:

```
Scan starts: pin at epoch 10
  ... read 1000 entries ...
  token.repin()          ← now at epoch 12, stale garbage reclaimed
  ... re-seek to last key ...
  ... read 1000 entries ...
  token.repin()          ← now at epoch 13
  ...
Scan ends: drop token
```

This keeps `local_epoch` close to the global epoch, allowing other reactors' `bump_epoch()`
calls to succeed without waiting for the scan to complete.

### Nested pin behavior

If `pin_count > 1`, repin updates `epoch_index` on the token but does **not** advance
`local_epoch` or reclaim garbage — an outer pin may still hold references from the
earlier epoch. Only the outermost pin (`pin_count == 1`) triggers reclamation.

```
let outer = domain.pin();       // pin_count = 1, epoch = 10
let inner = domain.pin();       // pin_count = 2, epoch = 10
inner.repin();                  // epoch_index = 12, but local_epoch stays 10
drop(inner);                    // pin_count = 1
outer.repin();                  // epoch_index = 12, local_epoch → 12, reclaim
drop(outer);                    // pin_count = 0
```

---

## Guard Trait Integration

The `Guard` trait in `starfish-core/src/guard/mod.rs` abstracts over reclamation
strategies. To support domains, we add a domain-aware guard type.

### Option A: DomainGuard (new type, Guard trait unchanged)

```rust
/// A Guard implementation pinned to a specific EpochDomain.
pub struct DomainGuard {
    domain: Arc<DomainHandle>,
}

impl Guard for DomainGuard {
    type GuardedRef<'a, T: 'a> = DomainRef<'a, T>;
    type ReadGuard = EpochToken;

    fn pin() -> Self::ReadGuard {
        // Problem: needs access to the domain, but pin() is a static method.
        // This doesn't work without additional context.
        todo!()
    }
    ...
}
```

**Problem:** `Guard::pin()` is `fn pin() -> Self::ReadGuard` — a static method with no
`&self` or domain parameter. There's no way to know *which* domain to pin.

### Option B: Guard trait gets domain context

Add a method that provides domain context from the guard stored in the collection:

```rust
pub trait Guard: Sized + Default + Send + Sync {
    type GuardedRef<'a, T: 'a>: Deref<Target = T>;
    type ReadGuard: Sized;

    /// Pin using the context of this guard instance.
    /// For domain-aware guards, this pins the domain stored in self.
    /// For EpochGuard (crossbeam), this ignores self and calls epoch::pin().
    fn pin(&self) -> Self::ReadGuard;  // &self instead of no-self

    unsafe fn defer_destroy<N>(&self, node: *mut N, dealloc: unsafe fn(*mut N));
    unsafe fn make_ref<'a, T: 'a>(ptr: *const T) -> Self::GuardedRef<'a, T>;
}
```

Collections already store a `G: Guard` instance. Changing `pin()` from associated
function to method lets domain-aware guards carry the domain reference:

```rust
pub struct DomainGuard {
    domain: *const EpochDomain,
}

impl Guard for DomainGuard {
    type ReadGuard = EpochToken;

    fn pin(&self) -> EpochToken {
        unsafe { &*self.domain }.pin()
    }

    unsafe fn defer_destroy<N>(&self, node: *mut N, dealloc: unsafe fn(*mut N)) {
        // Use the token from current pin context to defer within this domain.
        let token = unsafe { &*self.domain }.pin();
        // ... defer, token drops
    }
}
```

**This is the recommended approach.** The change from `fn pin()` to `fn pin(&self)` is
backwards compatible for callers that already have access to the guard (collections do).
`SortedCollection` trait methods already have `&self` access to the stored guard.

**Migration:** `EpochGuard` (crossbeam) and `DeferredGuard` add `&self` parameter but
ignore it — their `pin()` implementations are global, not instance-scoped.

---

## CooperativeInitializer Integration

Domains are dynamic, but the registry itself is initialized with the Coordinator.

```rust
impl CooperativeInitializer for Epoch {
    fn init(&self) {
        // Initialize default domain counters (as today).
        // Initialize domain registry.
    }

    fn init_reactor(&self, reactor: &Reactor) {
        // Set up default domain's counter for this reactor (as today).
        // Dynamic domains are joined lazily — no per-reactor init needed.
    }

    fn uninit_reactor(&self, reactor: &Reactor) {
        // Tear down default domain's counter (as today).
        // Tear down ALL domain counters for this reactor:
        //   iterate registry, null out this reactor's slot, free DomainCounter.
    }
}
```

### Reactor shutdown and dynamic domains

When a reactor shuts down (`uninit_reactor`), it must leave all domains it joined.
The registry provides an iterator over active domains. For each domain with a non-null
counter slot for this reactor:

1. Assert `pin_count == 0` (reactor shouldn't shut down while pinned).
2. Reclaim all 3 garbage bags.
3. Null out the counter slot (Release ordering).
4. Dealloc the `DomainCounter`.

---

## Example: Memtable Lifecycle

```rust
// Create a memtable with its own epoch domain.
let domain = Epoch::create_domain(Cadence::Flush);
let memtable: SkipList<MvccKey<K>, Option<V>, DomainGuard> =
    SkipList::with_guard(DomainGuard::new(&domain));

// Writes go into the memtable. Deletes defer nodes into the domain's bags.
// Epoch never advances — Flush cadence. No reclamation overhead.
memtable.insert(MvccKey::new(key, txn_id), Some(value));

// Flush to SST...
flush_to_sst(&memtable);

// Drop the memtable, then drop the domain.
// DomainHandle::drop() bulk-frees all garbage bags across all reactors.
drop(memtable);
drop(domain);  // all deferred allocations reclaimed in one sweep
```

## Example: Long Scan with Repin

```rust
let token = hot_index_domain.pin();
let mut cursor_key: Option<K> = None;

loop {
    // Re-seek from last key (or start).
    let iter = match &cursor_key {
        Some(k) => index.range_from(k, &token),
        None => index.iter(&token),
    };

    for (i, entry) in iter.enumerate() {
        process(entry);
        cursor_key = Some(entry.key().clone());

        if i % 1024 == 0 {
            token.repin();  // allow reclamation, invalidates iter
            break;          // must re-seek
        }
    }

    if done { break; }
}
drop(token);
```

---

## Phased Implementation

### Phase 1: EpochDomain + EpochToken (core)

Extract `EpochDomain` and `DomainCounter` from the existing `Epoch` and `EpochCounter`.
The existing `Epoch` becomes a wrapper around a default `EpochDomain`. Introduce
`EpochToken` with `repin()`. All existing tests pass unchanged.

- Refactor `Epoch` → `EpochDomain` + thin `Epoch` wrapper
- Rename `EpochGuard` → `EpochToken`, keep type alias
- Implement `repin()` on `EpochToken`
- Add tests for repin (single reactor, nested pins)

### Phase 2: Domain Registry + Dynamic Domains

Add `DomainRegistry`, `DomainHandle`, `Epoch::create_domain()`. Implement lazy
`join()` and reactor shutdown cleanup.

- `DomainRegistry` with atomic slab
- `DomainHandle` with RAII drop (bulk-free)
- `uninit_reactor` iterates all domains
- Tests: create/destroy domains, multi-reactor join/leave

### Phase 3: Cadence Policies

Implement `Cadence` enum and integrate each variant:

- `Manual`: already works (explicit `domain.bump()`)
- `Threshold`: inline check in `defer()`
- `Periodic`: reactor timer registration on join
- `Flush`: skip bump, bulk-free on drop
- Tests: each cadence variant, memory pressure scenarios

### Phase 4: Guard Trait Integration

Change `Guard::pin()` from associated function to `&self` method. Add `DomainGuard`.
Update all collection call sites.

- `Guard::pin()` signature change + migration
- `DomainGuard` implementation
- Update `SortedCollection`, `SkipList`, `SortedList` call sites
- Tests: collections with `DomainGuard`

---

## Open Questions

1. **Domain counter array sizing** — should the counter array be sized to
   `coordinator.reactor_count()` at domain creation time? Or use a growable structure
   for hot-add reactor support?

2. **Cross-domain deferred_delete** — what happens if a node allocated under domain A is
   deferred under domain B? Should we enforce domain affinity at the type level
   (token carries domain) or rely on debug_assert?

3. **Flush domain + deferred_delete** — in `Cadence::Flush`, `deferred_delete` still
   appends to garbage bags (tagged at the current epoch, which never advances). On
   `DomainHandle::drop()`, all bags are drained regardless of epoch. Should Flush
   domains skip the epoch tagging entirely and use a simpler append-only list?

4. **make_ref and domains** — `Guard::make_ref` is currently a static method (no `&self`).
   Does it need domain context? For `DomainGuard`, `make_ref` would need to pin the
   domain to protect the reference. This may require the same `&self` change as `pin()`.
