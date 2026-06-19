# Starfish-Epoch Bug Report

## Active findings (2026-06-05, multi-agent crate review)

Findings from a multi-lens review of `starfish-epoch` (7 reviewer lenses across all
files plus the reactor lifecycle it depends on; every finding adversarially verified
by an independent refute-by-default panel). 39 of 41 raised findings survived
verification. IDs continue above the historical max (prior bugs #1–#10 are resolved and
preserved in git history).

Baseline: `cargo clippy -p starfish-epoch --all-targets` is clean and all 9 integration
tests pass. Every item below is a latent bug, soundness/UB hazard, or coverage/doc gap —
none is a compile error or a currently-failing test.

---

## Critical

### BUG-11 (Critical, memory-safety): shutdown-drain use-after-free — `min_local_epoch()` can deref an `EpochCounter` a sibling reactor is concurrently freeing

**Files:**
- `src/epoch.rs:100-120` (`min_local_epoch`: `load(Acquire)` at :108, null-check at :109, `unsafe { &*ptr }` deref at :115)
- `src/epoch.rs:223-245` (`uninit_reactor`: store null at :230, `Box::from_raw` free at :240-242)
- `src/epoch.rs:215-221` (false shutdown-ordering comment)
- Lifecycle: `starfish-reactor/src/coordinator.rs:142-148, 172-191`; drain loop `starfish-reactor/src/reactor.rs:319-328`

`join_all()` sets `is_in_shutdown = true` on **all** reactors at once, then joins their
threads one at a time, with **no cross-thread barrier** between a reactor returning from
`run()` and that same thread entering `uninit_reactor()` (the only barrier in the system
is the *startup* `InitBarrier`). During shutdown the `run()` loop keeps polling
external/io/delayed/active futures until every queue drains (`reactor.rs:319-328`), and a
polled task can call `Epoch::bump_epoch()` → `min_local_epoch()`. `min_local_epoch`
loads each slot and, separately, dereferences it. Concrete race: reactor B empties its
queue first, exits `run()`, enters `uninit_reactor`, stores `null` (Release, :230) then
`Box::from_raw` **frees** its `EpochCounter` (whose `Drop` also drains all 3 bags); reactor
A's `load` at :108 was sequenced *before* B's null store, so A reads the stale non-null
pointer, passes the null-check, B frees, then A dereferences freed memory at :115 →
use-after-free + data race.

The Acquire/Release pairing does **not** save it: it only guarantees that *if* A observes
the null store it also observes B's prior writes — it creates no happens-before edge for
the case where A already read non-null, and nothing serializes A's deref (:115) against
B's free (:241). The SAFETY comment at :112-114 ("remains valid until uninit_reactor
stores null") is wrong (validity ends at the *free*, not the null store), and the comment
at :218-221 ("No reactor calls pin() or bump_epoch() during shutdown, so min_local_epoch()
is never invoked concurrently") is provably false given the drain loop.

Does not fire under current usage (default test pattern drains spawned futures before
`join_all`, so nothing calls `bump_epoch()` during the free window) and no test exercises
the interleaving — but it is reachable in any ≥2-reactor shutdown where one reactor's
queue empties while another task issues `bump_epoch()` during its drain. Note: only
`bump_epoch()` reaches `min_local_epoch()`; `pin()`/`local_epoch()` do not.

**Fix:** Make the counter lifetime structural — defer all `EpochCounter` frees to a
single-threaded phase *after* `join_all()` has joined every reactor thread, **or** add a
coordinator shutdown barrier so all reactors finish their `run()` drain before any counter
is freed (which would also make the :218-221 comment true), **or** make the free
epoch-/hazard-protected so the box outlives concurrent readers. A comment rewrite alone
does **not** make the code sound. This same free site underlies BUG-16 and BUG-17.

---

## Medium

### BUG-12 (Medium, concurrency/liveness): `repin()` on a non-innermost guard strands `local_epoch`, wedging reclamation and all `bump_epoch()`

**File:** `src/epoch.rs:507-525` (`repin`)

`repin()` gates and drives reclamation off the per-guard field `self.epoch_index`, but
unconditionally sets `self.epoch_index = global_epoch` (:521) even when `pin_count > 1`
(the reclaim + `local_epoch` advance at :517-518 run only when `pin_count == 1`). Trace
(single reactor): `outer = pin()` (idx=1, local=1); `inner = pin()` (pin_count=2);
`bump_epoch()` 1→2; `outer.repin()` while inner alive sets `outer.epoch_index = 2` but does
not advance `local_epoch`; `drop(inner)` → pin_count=1; `outer.repin()` again now sees
`global(2) == self.epoch_index(2)`, so the whole block is skipped and `local_epoch` is never
advanced. `local_epoch` is stranded at 1 for the outer guard's lifetime: `pin_count` never
hits 0, `min_local_epoch()` stays 1, and `bump_epoch()` (`1 < 2`) **blocks every further
bump on every reactor**, with stale bags never drained → unbounded growth. Not UB
(over-high `old_epoch` under-reclaims, never over-reclaims) and self-heals once the outer
guard drops, but it defeats repin's documented purpose. Reachable via the safe public API.

**Fix:** Drive the gate and reclaim from `counter.local_epoch`, not `self.epoch_index`:
```rust
if counter.pin_count == 1 {
    let local = counter.local_epoch.load(Ordering::Relaxed);
    if global_epoch != local {
        counter.generations.reclaim(local, global_epoch);
        counter.local_epoch.store(global_epoch, Ordering::Release);
    }
}
self.epoch_index = global_epoch; // always
```
Add a test that repins the outer guard while inner is alive, drops inner, repins outer, and
asserts `local_epoch` reaches `global` (see TST-3).

### BUG-13 (Medium, concurrency): epoch `u32` wraparound is not wrap-safe in the bump decision / bag selection (latent cross-reactor UAF at `MAX`→0)

**Files:** `src/epoch.rs:76` (`min_local < global`), `:105/:116` (`min()` seeded at `u32::MAX`); related `:88` (`wrapping_add`), `:365-371` (`reclaim` `wrapping_sub` + bag index)

The global epoch is advanced via `wrapping_add` and `reclaim` uses wrapping arithmetic —
proving wrap *was* intended to be supported — but the bump **decision** is not
wrap-consistent: `bump_epoch` uses plain `min_local < global` and `min_local_epoch` uses a
plain numeric `min()` seeded at `u32::MAX`. Two compounding problems at the boundary: (1)
right after global wraps `MAX`→0, a reactor still pinned at `local = MAX` is misread
(`MAX < 0` is false), so `bump_epoch` advances 0→1→2 while that reactor is stranded;
`reclaim(MAX, 2)` then replays `2.wrapping_sub(MAX).min(3) = 3` advances and frees bag 0,
which holds the epoch-`MAX` garbage the stranded reactor still references → cross-reactor
UAF, violating the E+2 invariant. (2) `2^32 % 3 == 1`, so wrapping arithmetic does not
preserve residue mod 3 across the wrap, and defer-slot vs reclaim-slot can select different
bags at the boundary (a live bag freed, a safe bag leaked). Requires ~2³² successful global
bumps — practically unreachable today (single default domain, explicit `Manual` bumps), and
all tests use epochs 1–3 — hence Medium, but a real latent gap in unsafe code whose cheap
fix removes an inconsistency with the deliberately wrap-safe `reclaim`/`wrapping_add`.

**Fix:** Make the comparison wrap-relative (caught-up ⇔ `global.wrapping_sub(min_local) == 0`,
lagging ⇔ a small positive wrapping distance) and replace numeric `min()` with a wrap-aware
minimum; **or** switch `current_epoch_index`/`local_epoch` to `AtomicU64` so wrap is
unreachable in practice and document the assumption. Add a unit test seeding epochs near
`u32::MAX`.

### BUG-14 (Medium, memory-safety): `EpochAlloc::realloc` forwards an un-rounded `new_size` while `dealloc` re-pads it — latent `GlobalAlloc` layout inconsistency

**File:** `src/epoch_alloc.rs:44-48` (`realloc`) vs `pad()` at `:58-75`

`alloc`/`alloc_zeroed`/`dealloc` all route through `pad()`, which rounds size up to a
multiple of the padded align (:73). `realloc` computes `new_size = new_size.max(16)` and
forwards it **without** that round-up (applying `pad` only to the *old* layout). Per the
`GlobalAlloc::realloc` contract the caller then records the block as `Layout(new_size,
align)`, and at free `dealloc` re-pads, rounding up — so for the same block EpochAlloc tells
its backend `new_size` on realloc but a larger padded size on free. Example: old=(8,8),
`new_size=20` → realloc passes 20, later dealloc passes `pad((20,8)) = 24`. Non-8-multiple
sizes are routinely produced by `Vec<u8>`/`String` growth, and this is the **process-global**
allocator, so the path is exercised constantly. Benign **only** because mimalloc-0.1.48's
`dealloc` is `mi_free(ptr)` (discards the layout); it becomes a mismatched-dealloc → UB the
moment the backend is swapped for a size-validating allocator or a sized-free path. (The
align half is dead code: Rust caps alignment at 2²⁹, so `align as u32` cannot truncate.)

**Fix:** Round `new_size` symmetrically before forwarding so the realloc-time size matches
what `dealloc` will re-pad to:
```rust
let new_layout = Self::pad(Layout::from_size_align(new_size, layout.align()).unwrap());
GLOBAL_MI_ALLOC.realloc(ptr, Self::pad(layout), new_layout.size())
```

---

## Low

### BUG-15 (Low, memory-safety): release builds silently truncate `> u32::MAX` object sizes in `DeferredList::append`

**File:** `src/epoch.rs:163-182` (`deferred_delete`), `:289-290` (`size as u32`)

The u32-fit guard at :169-174 is `debug_assert`-only while the truncating cast at :289 is
unconditional. A type with `size_of::<T>() > 4 GiB` would, in release, store a truncated
size and reconstruct a wrong `Layout` for `dealloc` — a mismatched-dealloc. No present UB
(mimalloc's `mi_free` ignores the size; backend hardcoded to MiMalloc) and unrealistic for a
crate reclaiming small intrusive nodes, but the precondition is also undocumented (the
`# Safety` block lists ZST/non-null/pinned but not the u32-fit requirement). Same underlying
class as BUG-14.

**Fix:** Promote the size guard to a hard `assert!` (or saturate), and add the u32-fit
precondition to the `# Safety` doc on `deferred_delete`.

### BUG-16 (Low, memory-safety/liveness): a reactor-thread panic skips `uninit_reactor`, leaking the `EpochCounter` and stalling survivors

**File:** `src/epoch.rs:206-245` vs `starfish-reactor/src/coordinator.rs:142-148`

A panicking future unwinds out of `run()` before `uninit_reactor` runs (no `Drop`/scope
guard; the workspace uses `panic = "unwind"`). The `Box::leak`'d `EpochCounter` and all
deferred allocations in its 3 bags are leaked, and `counters[idx]` is left pointing at the
leaked (not freed) counter. A surviving reactor's `min_local_epoch()` then reads the dead
reactor's **frozen** `local_epoch`, which can pin global-epoch advancement and stall
reclamation for survivors. The slot is stale-but-valid (leaked, not dangling) — a
leak/liveness bug, not UB.

**Fix:** Null the slot + reclaim in a `Drop`/scope guard around `run()` (or have the
coordinator null the slot from the join/shutdown path on panic), and document the
panic-leak behavior. Shares the free site with BUG-11/BUG-17.

### BUG-17 (Low, memory-safety): `EPOCH_COUNTER_INSTANCE` is never reset and `local_instance()`'s SAFETY comment is wrong

**File:** `src/epoch.rs:437-445` (`local_instance`), `:141-148` (`reset_local_instance`), `:530` (guard `Drop`)

`local_instance()` does `&mut *ptr` with no null/dangling check, and its SAFETY comment at
:438-440 falsely claims the pointer is "cleared by reset_local_instance" — but
`reset_local_instance` only nulls `EPOCH_INSTANCE`, never `EPOCH_COUNTER_INSTANCE`, so after
`uninit_reactor` frees the counter the TLS holds a dangling pointer. No reachable UB today
(`EpochGuard` is `!Send`; all callers run between `init_reactor` and the free, and full
queue-drain precedes the free), but the wrong comment plus missing hardening is worth fixing.

**Fix:** Reset `EPOCH_COUNTER_INSTANCE` to null in `uninit_reactor`, add
`debug_assert!(!ptr.is_null())` in `local_instance()`, and correct the comment. Closely
related to BUG-11 (same free site).

---

## API / design

### CLN-1 (Medium, api/design): process-global `#[global_allocator]` is an undocumented, unguarded soundness precondition of `deferred_delete`

**Files:** `src/epoch_alloc.rs:21-22`; `src/epoch.rs:163-182` (`deferred_delete`), `:280-324` (`DeferredList::append`/`reclaim`)

The whole reclamation scheme is unsound unless `EpochAlloc` actually serviced the
allocation: `append` writes a 16-byte `DeferredAllocation` node into the freed object's own
memory, valid only because `pad()` guarantees ≥16 bytes / ≥8-byte align. A
`#[global_allocator]` is a process-wide, link-time, unchecked property, and
`deferred_delete`'s contract (:157-158) requires the pointer be allocated via `EpochAlloc`,
but nothing ties the API to the allocator at compile or run time. No concrete trigger exists
today (`deferred_delete` has no non-test callers, and every binary that calls it links
starfish-epoch and thus installs `EpochAlloc`), so it is a latent design/coupling hazard,
not an active bug.

**Fix:** Document prominently (crate `//!` and `deferred_delete`'s `# Safety` section) that
linking starfish-epoch installs `EpochAlloc` as **the** process allocator and that
`deferred_delete` is unsound otherwise. Better: gate the `#[global_allocator]` behind a cargo
feature so the coupling is opt-in and visible. Consider a debug-build canary in `append()`.

### CLN-2 (Low, robustness): no static assertion that `DeferredAllocation` still fits `MIN_ALLOC_SIZE`

**Files:** `src/epoch.rs:262-267` (`DeferredAllocation`), `src/epoch_alloc.rs:15,63` (`MIN_ALLOC_SIZE`/`MIN_ALLOC_ALIGN`)

`MIN_ALLOC_ALIGN` is derived from the struct but `MIN_ALLOC_SIZE` is hardcoded to 16, so
adding a field would silently grow the node to 24 bytes and make `append()` write past a
16-byte block → heap corruption, with nothing failing the build.

**Fix:** Add a compile-time guard, e.g.
`const _: () = assert!(size_of::<DeferredAllocation>() <= MIN_ALLOC_SIZE && align_of::<DeferredAllocation>() <= MIN_ALLOC_SIZE);`

---

## Docs

### DOC-1 (Low, docs): shutdown SAFETY comments give inaccurate justifications

**Files:** `src/epoch.rs:215-221` and `:232-234`

The :218-221 comment ("never invoked concurrently") is false (see BUG-11). The :232-234
comment ("all event loops have returned") is also not the real reason the bag drain is safe —
the true invariant is **exclusive per-reactor ownership** of the garbage bags (only ever
touched via the thread-local `local_instance()`; `min_local_epoch` reads only `local_epoch`,
never `generations`). Sound-code/wrong-comment issues.

**Fix:** Rewrite both comments to state the true invariants. (Also tighten the secondary doc
nits: `deferred_delete` tags garbage at the reactor's `local_epoch`, not the calling guard's
`epoch_index`, :180; and the "same as crossbeam `defer_unchecked` + dealloc" analogy at
:150-153 / `README.md:53` is imprecise — `deferred_delete` defers only deallocation, the
`Drop` runs synchronously at call time.)

### DOC-2 (Low, docs/design-drift): `epoch-domains.md` says `NOT IMPLEMENTED` though `repin()` shipped and is tested

**Files:** `docs/epoch-domains.md:6` & `:576-587`; `docs/DESIGN-INDEX.md`; `src/lib.rs:10-17`

`repin()` (a documented Phase-1 deliverable) is implemented on `EpochGuard`
(`src/epoch.rs:507-525`) and tested (`tests/epoch_test.rs:288-338`), but the design-doc
status header still reads `NOT IMPLEMENTED`, the `lib.rs` TODO doesn't record repin as done,
and the README doesn't mention it. The `EpochDomain`/`EpochToken`/registry refactor was
genuinely not done. Correct state: **PARTIALLY IMPLEMENTED**.

**Fix:** Update the status to `IN PROGRESS` (or split the doc), and reconcile the Phase-1
checklist and the `lib.rs` TODO.

---

## Tests

### TST-1 (High, tests): no multi-reactor test verifies cross-reactor reclamation at the E+2 boundary

**File:** `tests/epoch_test.rs:164` (`epoch_multi_reactor_bump_blocked_by_lagging_reactor`)

The only multi-reactor test exercises bump-*gating* (a lagging pin blocks a second bump); it
never calls `deferred_delete` and never asserts reclamation. Every reclamation test runs
under `initialize(1)`, where `min_local_epoch()` trivially returns the single reactor's own
epoch — so the crate's headline guarantee (garbage tagged at E on reactor A is freed only
after **all** reactors observe E+1, then exactly once) has zero coverage. This is the test
that would surface BUG-11 and BUG-12 under a sanitizer.

**Fix:** Add `epoch_multi_reactor_reclamation_respects_e_plus_2`: 2 reactors; reactor 0
defers an observable object and bumps to E+1; reactor 1 holds an epoch-E pin so reactor 0's
E+1→E+2 bump must fail; release reactor 1, catch up to E+2, re-pin, and assert the object is
freed exactly once via a dealloc hook or heap canary (not via the `Drop` counter — see
TST-2). Ideally run the multi-reactor shutdown stress under TSan/ASan.

### TST-2 (Medium, tests): `epoch_reclamation_frees_memory` asserts on `Drop` count, not the dealloc side-effect

**File:** `tests/epoch_test.rs:225-285`

`DROP_COUNT` is incremented only by `Tracked::drop`, which fires at the test's own manual
`drop_in_place` calls (:249, :275) — **before** any epoch advance. The reclamation path
(`DeferredList::reclaim` → `dealloc`) runs no destructor, so it can never change
`DROP_COUNT`; both assertions (`== 3` at :256, `== 4` at :280) hold regardless of whether
reclamation ever ran, reducing the test to "didn't crash." The doc comment "Verifies that
deferred allocations are actually freed by observing a side-effect" overclaims. Sibling
`epoch_reclamation_after_two_advances` shares the weakness.

**Fix:** Observe the dealloc, not the drop — wrap the allocator in a counting shim and assert
the dealloc count rises only after reaching E+2, or expose a test-only bag-length accessor
and assert non-empty-before / empty-after the reclaiming pin.

### TST-3 (Medium, tests): `repin()`'s reclaim-while-pinned side-effect is untested

**File:** `tests/epoch_test.rs:288 & 310` vs `src/epoch.rs:507-525`

repin's purpose is to reclaim stale garbage without unpinning (the `generations.reclaim()`
call at :517, gated on `pin_count == 1`), but both repin tests defer no garbage, so deleting
:517 still passes both tests. The `pin_count == 1` safety gate has only its bookkeeping
checked, never the actual "object not yet freed" invariant. Directly related to BUG-12.

**Fix:** Add `epoch_repin_reclaims_stale_garbage` (defer an observable object, advance to
E+2, repin the outermost guard, assert it is freed during repin) plus the negative
`pin_count == 2` case asserting non-reclaim.

### TST-4 (Low, tests): shutdown bag-drain frees nothing observable in any assertion

**File:** `src/epoch.rs:223` / `tests/epoch_test.rs`

Two existing tests do leave non-empty bags at shutdown, so the `Box::from_raw` →
`DeferredList::drop` → `reclaim` drain path *is* exercised — but no test asserts via an
observable dealloc side-effect that the drain actually frees (same assertion-strength gap as
TST-2).

**Fix:** Add `epoch_shutdown_drains_pending_bags` asserting deallocation via a counting
allocator shim.

### TST-5 (Low, tests): allocator paths `realloc` / `alloc_zeroed` / over-aligned `pad` are untested

**Files:** `src/epoch_alloc.rs:32-48`, `pad` `:58-75`

No test grows a `Vec`/`String` (realloc — where BUG-14 lives), allocates zeroed, or defers an
over-aligned (`#[repr(align(32/64))]`) type, so the size-round-up branch of `pad` and the
symmetric re-pad in `dealloc` are never exercised.

**Fix:** Add a pure unit test of `pad()` (assert size is always a multiple of align;
`(1,1) → (16,8)`; `(8,64) → (64,64)`), an over-aligned `deferred_delete` roundtrip, and a
growing-`Vec` test — ideally under Miri, which (unlike MiMalloc) flags layout-contract
mismatches.

### TST-6 (Low, tests): `deferred_delete`'s `debug_assert` preconditions (ZST, no-pin, oversized) have no negative tests

**File:** `src/epoch.rs:163-182`

None of the ZST, `pin_count > 0`, or u32-fit guards is exercised. These are debug-only
contract guards, so a test only verifies the assertion fires.

**Fix:** Add debug-build negative tests — note the no-pin case must run inside a spawned
reactor task and assert `join_all().is_err()` (a panic on the reactor thread is captured by
`join()`), not a plain `#[should_panic]`; the oversized case is untestable in practice.
