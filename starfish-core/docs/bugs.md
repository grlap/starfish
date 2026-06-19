# starfish-core Bug Tracker

**Active bugs only.** Fixed entries are REMOVED (git history preserves them — `git log -p` this
file); durable engineering insight from fixed bugs lives in code docs (`//!` headers, doc
comments, `// SAFETY:` comments) per the project rule, NOT here. IDs are historical and
non-contiguous: a missing number means that bug was fixed and its entry deleted. Code comments
may cite an ID only while it is listed below; when fixing a bug, scrub its ID from code
comments (keep the insight, drop the pointer).

Validation requirements for anything touching lock-free reclamation are in `CLAUDE.md`
("Lock-free reclamation validation gate").

---

### BUG-8 (High, memory-safety): `SortedCollection` iterators let `CollectionRef` outlive the read guard (use-after-free in safe code)

**Files:**
- `src/data_structures/internal/sorted_collection.rs:622-637` (`SortedCollectionIter::next`)
- `src/data_structures/internal/sorted_collection.rs:765-805` (`SortedCollectionRange::next`)

Both iterators declare `type Item = CollectionRef<'a, T>` tied to the *collection-borrow*
lifetime `'a`, but reclamation protection is the iterator's own `_guard` field (it pins the
thread only while the iterator is alive). A caller can move the yielded ref out, drop the
iterator (unpinning the epoch), then dereference a node that reclamation is now free to free.
Reproduced against `SkipList<i32, EpochGuard>`:
`let escaped; { let mut it = list.iter(); escaped = it.next(); } println!("{}", *escaped.unwrap());`
compiles and runs after the guard is dropped; `range(0..100)` has the identical flaw. `find()`
is unaffected because `EpochRef`/`DeferredRef` bundle their own guard. The SAFETY comment at
:627-629 asserts a guarantee the signature does not provide.

**Fix:** Tie the yielded ref's lifetime to the iterator's pin, not the collection borrow.
Either (1) change `Item` to the guard's `GuardedRef` produced via `make_ref` (as `find()`
does) so each yielded ref bundles its own pin, or (2) make the iterators lending (return
`CollectionRef<'_, T>` borrowing `&mut self` via a LendingIterator/GAT pattern) so the borrow
checker forbids escape. Correct the SAFETY comment at :627-629 either way.

### BUG-9 (High, memory-safety): `MapCollectionInternal` guard-sensitive methods are safe `fn` returning/dereferencing raw node pointers

**File:** `src/data_structures/hash/map_collection.rs:65,70,75,79,90`

Every pointer-bearing `MapCollectionInternal` method is a safe `fn`:
`insert_internal`/`remove_internal`/`find_internal`/`update_internal` return `*mut Node` from
lock-free traversal and `apply_on_internal` takes and dereferences a `*mut Node`, yet every
impl depends on a pinned read guard. This is exactly the soundness hazard already addressed on
the `SortedCollectionInternal` side (its guard-sensitive methods are `unsafe fn` with a
"# Safety: caller must hold a pinned read guard" contract) but never propagated to the Map side. Combined with CLN-28 (the trait is
publicly reachable), a downstream crate can in 100% safe code call `find_internal` without a
pin, have the node reclaimed by a concurrent `remove`, then call `apply_on_internal(n)` doing
`&*node` on freed memory. `apply_on_internal` is doubly unsound: any garbage `*mut Node` passed
to a safe signature is instant UB.

**Fix:** Make `insert_internal`, `remove_internal`, `find_internal`, `apply_on_internal`,
`update_internal` (and `len_internal` if it traverses) `unsafe fn` with a "# Safety: caller
must hold a pinned read guard for the duration of the call and as long as any returned pointer
is used" contract, mirroring `SortedCollectionInternal`. Wrap the already-pinning
`MapCollection` default-method call sites in `unsafe { }` and update the impls in
`sorted_list.rs`, `skip_list.rs`, `skip_trie.rs`, `split_ordered_hash_map.rs`.

*Status note (2026-06-10):* the former BUG-31 (`MapNode::value()` null-deref on tombstones) was
narrowed — tombstones now return `None` — but its residual unpinned-`&V` hazard is this bug:
any fix must cover `value()`/`apply_on_internal` returning borrows without a pin obligation.

### BUG-11 (High, concurrency): `SkipTrie` delete/update use the un-fixed BUG-5 reachability gate, surfacing as wrong (false) results

**Files:**
- `src/data_structures/trie/skip_trie.rs:1085` (`delete_from`)
- `src/data_structures/trie/skip_trie.rs:1407` (`update_from`)

`delete_from` and `update_from` gate on `if height > 0 && succs[height] != curr { return None; }`
— the exact top-level-reachability "fully linked" test BUG-5 replaced in SkipList with a per-node
publication flag. `SkipTrieNode` has no `is_fully_linked`/`linked_height` flag, so the fix was
never propagated. A node whose upper levels are transiently unlinked by a concurrent mutator
yields a spurious `None` even though the key is present at level 0 the whole time. Because public
delete/update (`internal/sorted_collection.rs:258,308,279`) make a single attempt with no retry,
this surfaces to the caller as `delete(&K) == false` / `update == false` for a present key — a
linearizability violation. This is the SkipTrie analog of the reopened **BUG-5**.

**Fix:** Add a per-node publication signal to `SkipTrieNode` (set once `insert_from` finishes
linking all levels) and replace the `succs[height] != curr` reachability gate in both
`delete_from` (:1085) and `update_from` (:1407) with a check on the node's own publication state,
distinguishing "still being inserted" (bail) from "fully published but transiently unreachable at
upper levels" (proceed). Alternatively make public delete/update retry on the spurious-None path.
**Template:** SkipList's fix for the identical gate (BUG-5, since removed as fixed; protocol
revised 2026-06-10): there is NO reachability/publication gate at all. Presence is the level-0
mark alone (`SkipNode::is_visible`) for find, insert's duplicate decision, iteration, AND
delete's eligibility — transient upper-level unlinks cannot fake "absent". Delete linearizes at
the level-0 mark CAS with no precondition; the *epoch-safety* requirement (full-tower
marking/unlink must not race in-flight insert CASes or plain tower stores) is enforced on the
PHYSICAL phase only, via the `linked_height` RMW handshake (inserter `swap(height)` vs deleter
`fetch_or(DEL_PENDING)` — exactly one runs `physical_delete`, nobody waits). For SkipTrie either
port that split or, more simply, replace the `succs[height] != curr` gate with a per-node
publication flag plus retry on the spurious-None path.

### BUG-14 (High, correctness): `YFastTrie::remove()` that wins its CAS but loses the split race returns `None` for a key it deleted and leaks `count` +1

**File:** `src/data_structures/trie/y_fast_trie.rs:608-650` (split-race branch at :638-647; decrement at :616; undo at :645)

When `remove`'s bucket CAS succeeds (key physically removed, `count.fetch_sub` at :616) and a
concurrent split then freezes the post-removal Vec, the split partitions data that no longer
contains the key. `remove` sees the generation changed (:638), undoes its decrement via
`count.fetch_add(1)` (:645), and retries; the retry lands in the correct child bucket, does not
find the key (genuinely gone), and returns `None` (:633->650). Net: the operation that actually
deleted the key reports `None` (linearizability violation — a concurrent reader saw
present-then-absent) and `self.count` is permanently +1 too high. The symmetric insert path
(`prior_cas_raced`) is correct; remove has no equivalent.

**Fix:** In the split-race branch, when `result.is_some()` and the generation changed, treat the
removal as committed (a successful inner CAS implies inner was non-null, so it preceded the freeze
and is in the frozen snapshot): do NOT undo the count, and return the captured `Some(value)`
directly rather than retrying. Only restore-and-retry/return-None when `result.is_none()`. This
mirrors the insert-side `prior_cas_raced` reasoning.

### BUG-15 (High, correctness): `YFastTrie` key-bit masking collapses distinct in-range keys to the same routing bits (silent key loss)

**File:** `src/data_structures/trie/y_fast_trie.rs:439-446` (`key_to_bits`), :799 (`median_bits`), :872-875 (rep insert)

`key_to_bits` masks `key.to_bits()` to the bottom `key_bits` (30 for the 2^30 default universe).
For `i32`, `to_bits()` flips the sign bit, so two distinct in-range keys can mask to the same
routing value: key `0 -> 0x80000000 -> masked 0`, and key `1073741824` (valid i32)
`-> 0xC0000000 -> masked 0`. Routing (`find_successor`) and rep-table keys (`median_bits`,
`binary_search_by_key`) use masked bits while in-bucket storage uses the full key K. When two
mask-colliding keys straddle a split boundary, the larger lands in the right bucket but both still
route to the left bucket, so the larger becomes unreachable (silent key loss); the colliding
`median_bits` can also make the reps Vec non-monotonic, breaking the binary-search precondition.

**Fix:** Route and key the rep table on the full order-preserving `to_bits()` (u64) rather than
the masked value, OR validate that all keys fall within `[0, 2^key_bits)` and reject otherwise, OR
derive `key_bits` from `K::BIT_WIDTH` so the mask is the identity for the key type.

### BUG-16 (High, correctness): `SplitOrderedHashMap::with_hasher_and_capacity` accepts non-power-of-two bucket count, breaking split-order reachability (silent data loss)

**File:** `src/data_structures/hash/split_ordered_hash_map.rs:271-332` (`bucket_size` stored at :327; bucket math at :555/584/605/649; resize at :483)

The public constructor stores `initial_buckets` verbatim into `bucket_size` with no power-of-two
rounding or validation. The split-ordered scheme requires `bucket = hash % bucket_size` to equal
the low bits of `hash`, which only holds for power-of-two sizes; otherwise a regular entry's
`split_key` (`reverse_bits(hash)|1`) can sort BEFORE its bucket sentinel's `split_key`
(`reverse_bits(bucket)&!1`), so find/contains/remove starting at the sentinel walk past the entry
and report it absent, and insert re-inserts duplicates. Empirically `bucket_size=10` produces ~64
reachability violations over hashes 0..200 (0 for power-of-two). `resize()` only doubles (:483),
so a bad seed never self-corrects. Additionally `initial_buckets > MAX_BUCKETS (1<<16)` makes
`get_bucket_sentinel` return `None` (silent dropped write) or panic in `initialize_bucket`.
Reachable purely through public API; the default path (`INITIAL_BUCKETS=16`) is safe.

**Fix:** Normalize and bound the capacity:
`let initial_buckets = initial_buckets.max(1).next_power_of_two().min(MAX_BUCKETS);` (or
assert/return `Result` on non-power-of-two and on `> MAX_BUCKETS`). Document the power-of-two
requirement on the public fn.

### BUG-17 (High, correctness): Re-inserting an existing `Treap` key with a higher priority corrupts the min-heap invariant (no sift-down)

**File:** `src/data_structures/sorted/treap.rs:93-98` (`Equal` branch); `get_min_priority_node` at :203-210

In `insert_rec`'s `Ordering::Equal` branch the code overwrites value and priority and returns the
node unchanged, never re-establishing the min-heap invariant. The recursive path only performs
UPWARD bubbling (`rotate_right`/`rotate_left` when a child's priority is lower); there is no
sift-DOWN. So re-inserting an existing key with a priority HIGHER than one of its children leaves
`parent.priority > child.priority` permanently. Consequences: the O(log n) balance guarantee is
broken, and `get_min_priority_node()` (which assumes the root holds the global minimum) returns an
incorrect `(key, value, priority)`. Reproduced: insert (10,10),(20,20),(30,30) then insert(10,_,99)
yields "key 20 prio 20 < parent prio 99" and `get_min_priority_node` returns (10,99) instead of the
true min (20,20). The lowering case is repaired by the existing upward cascade; the raising case is
not.

**Fix:** After updating priority in the `Equal` branch, sift down: while either child has a
strictly lower priority, rotate the lower-priority child up and continue into the subtree now
holding the node. Alternatively implement priority update as remove-then-insert. Add a test
re-inserting an existing key with both higher and lower priority asserting BST + min-heap
invariants and `get_min_priority_node` correctness.

### BUG-19 (Low, memory-safety): `SkipTrie` Drop-walk safety reasoning contradicts deferred-destroy of UPDATE-marked old nodes (latent double-free)

**File:** `src/data_structures/trie/skip_trie.rs:2199-2235` (Drop); unlink at :1471-1481; `defer_destroy` at `internal/sorted_collection.rs:317` / `hash/map_collection.rs:192`

`SkipTrie::drop` walks the level-0 chain and `dealloc_nodes` every node reached; comments
(:2208-2210, :2219) assert UPDATE-marked OLD nodes are still in the chain and intentionally freed
by this walk. But every UPDATE old node is both physically unlinked at level 0 by `update_from`
Step 5 (:1471-1481) AND deferred-destroyed by the wrapper. The `DeferredGuard` duplicate check only
detects a node registered twice via `defer_destroy`, NOT a node freed once by the Drop-walk and
once via the deferred list, so an UPDATE-marked node still chained at drop would be a silent
double-free. Currently latent, but the no-double-free property rests on an unstated, unchecked
precondition that contradicts the stated Drop invariant.

**Fix:** Correct the Drop comment to state the real invariant (at drop time no deleted node and no
UPDATE-marked OLD node should remain reachable at level 0). Add a `debug_assert` in the Drop-walk
that each visited node's `next[0]` is NOT update-marked (mirroring the existing delete-mark panic
at :2220-2225) so the double-free case is enforced rather than assumed.

*Audit note:* the in-code Drop comments at `skip_trie.rs:2209,2219` are themselves inaccurate
and should be corrected alongside this fix.

### BUG-21 (Low, memory-safety): `DeferredGuard::drop` panics before freeing, leaking queued nodes and risking double-panic abort

**File:** `src/guard/deferred_guard.rs:55-93` (panic at :82, free loop at :85-92)

`Drop` runs a duplicate-pointer scan and panics at :82 BEFORE the drain/free loop (:85-92), so on
a detected duplicate none of the queued nodes are deallocated (the Vec drop frees only the
`DeferredNode` structs, not the target allocations) — every pending node leaks. Worse, panicking
inside a `Drop` that itself runs during unwinding double-panics and aborts the process. Since the
stress tests all use `DeferredGuard`, a real double-free during a test surfaces as an abort/leak
rather than a clean failure, and can mask the real error.

**Fix:** Do not panic inside `Drop`. Collect duplicate diagnostics, still free each distinct
pointer exactly once (skip already-freed addresses), and surface duplicates via
`eprintln!`/`debug_assert!` rather than an unconditional panic; or free all distinct pointers first
and only then assert. Gate any panic behind `if !std::thread::panicking()`.

### BUG-22 (Low, correctness): `CountdownEvent::signal()` returns "reached-zero" but is consumed as a double-signal detector

**File:** `src/preemptive_synchronization/countdown_event.rs:26-38`; consumer `starfish-reactor/src/completion_signaler.rs:26-27`; `new(1)` at `reactor.rs:589`

`signal()` returns `true` ONLY on the decrement that drives count to exactly zero, and `false` for
every earlier valid decrement and for over-signaling an already-zero counter — i.e. it means "this
signal reached zero", not "this signal was effective". `completion_signaler.rs` treats the return
as a double-signal detector via `debug_assert!(signaled, ...)`, which is only correct because the
preemptive path always constructs `new(1)`. For any initial count `> 1` the same pattern fires a
false-positive assert on every non-final signal. The boolean's contract is also undocumented.

**Fix:** Document `signal()`'s boolean precisely (true iff this call transitioned the count to
zero) and decouple the double-signal check (expose whether a decrement actually occurred, or add a
`was_already_zero`/`Result` channel). At minimum, comment `completion_signaler.rs:26-27` that the
assertion relies on the latch being `new(1)`.

*Cross-crate touchpoint:* `starfish-reactor` consumes `CountdownEvent::signal()` in
`completion_signaler.rs` (and lacks `catch_unwind` around task polls — reactor tracker #55), so
any change to the `signal()` contract must be validated against the reactor's completion path.

### BUG-34 (Medium, concurrency): `SplitOrderedHashMap` bucket-sentinel initialization spin-waits on another thread's progress

**Files:** `src/data_structures/hash/split_ordered_hash_map.rs:383,418` (`std::hint::spin_loop()` sites)

Both sites wait for ANOTHER thread to finish initializing a bucket sentinel — an unbounded wait
if that thread is preempted (a spin lock in disguise; same class as the SkipList insert/map
spin-waits removed 2026-06-10, and contrary to the crate's no-waiting rule). **Fix:** helping —
a thread that finds the sentinel mid-initialization should complete the initialization itself
(idempotent CAS-publish of the sentinel), mirroring how the skip-list teardown is helped.

### CLN-28 (Medium, api): `MapCollectionInternal` is publicly reachable from downstream crates, contradicting its "crate-private" doc

**File:** `src/data_structures/hash/map_collection.rs:44-47`; re-export chain `lib.rs:8 -> data_structures/mod.rs:25 -> hash/mod.rs:6`

`MapCollectionInternal`'s doc claims it cannot be named by external crates, but the whole chain is
public: `lib.rs:8 pub mod data_structures`, `mod.rs:25 pub mod hash`,
`hash/mod.rs:6 pub use map_collection::{... MapCollectionInternal ...}`, so
`use starfish_core::data_structures::hash::MapCollectionInternal;` compiles downstream. This is the
asymmetric opposite of `SortedCollectionInternal`, which is correctly hidden behind
`pub(crate) mod internal`. The leaked trait exposes the guard-sensitive primitives of BUG-9 to
external code, and the doc comment is simply false.

**Fix:** Mirror the `SortedCollectionInternal` design: move `MapCollectionInternal` into the
`pub(crate)` internal module, or change `hash/mod.rs:6` to `pub(crate) use` for
`MapCollectionInternal` (keeping `MapCollection` and `MapNode` pub). Remove or correct the false
doc claim at :44-46.

### CLN-29 (Low, api): `SortedCollectionIter::from_node` is a safe `pub` constructor storing an unchecked raw node pointer

**File:** `src/data_structures/internal/sorted_collection.rs:601-609`

`pub fn from_node(collection, node: Option<*mut C::Node>)` accepts an arbitrary raw node pointer
and stores it as `current_node`, which `next()` unconditionally dereferences via `(*node).key()`
(:635). It is safe (no `unsafe` qualifier) and has no `# Safety` section, yet soundness depends on
the caller passing a guard-protected valid pointer. Blast radius is limited (enclosing module is
`pub(crate)`; no in-crate callers today), so it is a latent internal-contract hole, but it violates
the crate convention of marking guard-sensitive raw-pointer primitives `unsafe fn`.

**Fix:** Mark `from_node` `unsafe fn` with a `# Safety` section requiring that `node` (if `Some`)
was produced by this collection and is valid/reachable under a guard pinned no later than this
call, or remove the method if it has no callers.

### CLN-30 (Low, architecture): `IterableCollection` / `IterableSortedCollection` are public, documented, but have zero implementors

**File:** `src/data_structures/iterable_collection.rs:23-62`; re-export at `data_structures/mod.rs:37`

Both traits are exported public API and advertised in `README.md:127-128` and design docs, but no
type in starfish-core/-crossbeam/-epoch implements either (grep finds only declarations and
doc/README mentions). Actual iteration is provided entirely via
`SortedCollection::iter`/`SortedCollectionIter`. The module `//!` admits the traits are speculative
and references a non-existent `SafeSortedCollection` type (:7). This is a parallel never-wired
iteration abstraction presenting an empty public contract and creating confusion about the
canonical iteration API.

**Fix:** Either implement these traits for the existing collections so the public API is real, or
demote them to `pub(crate)`/remove the re-export at `mod.rs:37` and the README entries until wired
up. Fix the stale `SafeSortedCollection` reference in the module `//!` doc.

### CLN-32 (Low, memory-safety): `MapEntry::value_ref`/`value_ptr` have no null guard

**Files:** `src/data_structures/map_entry.rs` (`value_ref`, `value_ptr`).

The "value is never null for a live entry" invariant rests entirely on caller discipline
(`find_node_internal` never returns the sentinel). Not currently exploitable, but unlike `Drop`
and `get_ref` there is no guard. **Fix:** `debug_assert!(!self.value_ptr().is_null())` to fail
loudly if a future caller ever reaches it on a value-less node.

### CLN-33 (Low, cleanup): confirm node-replacement-only helpers are not dead after the UPDATE removal

**Files:** `skip_list.rs` (`take_value_unlinked`, and any `insert_at_level`/`unlink_at_level` paths
formerly used only by the removed `update_internal`).

The neutralized `update_internal` no longer calls its former helpers; the build is warning-clean
(so they remain referenced by DELETE/insert), but a grep should confirm none are now dead.
**Fix:** grep callers; remove anything unused.

### DOC-2 (Medium, docs): README `MapCollection` trait documentation lists crate-private `MapCollectionInternal` methods as if they were the public API

**File:** `README.md:98-120` vs `map_collection.rs:47-95` (Internal) and :113-213 (public)

The "Core Traits" section documents `MapCollection<K,V>` with
`guard()`/`insert_internal`/`remove_internal`/`find_internal`/`update_internal`/
`apply_on_internal`/`len_internal`, but in code those `_internal` raw-pointer methods live on the
crate-private `MapCollectionInternal`, while public `MapCollection` contains only the safe
convenience methods. The README presents the private guard-sensitive surface as the public trait —
the inverse of the documented safe/unsafe split on the `SortedCollection` side.

**Fix:** Rewrite the section to mirror the `SortedCollection` section: document
`MapCollectionInternal` as the crate-private low-level trait (associated types `Guard`/`Node`;
`_internal` methods + `guard`), and document public `MapCollection` as the safe wrapper
(`insert`/`remove`/`contains`/`get`/`update`/`find_and_apply`/`is_empty`/`len`).

### DOC-3 (Medium, docs): README usage examples import from non-existent module paths and will not compile

**File:** `README.md:165-166` (repeated at :188-190)

Both examples import `use starfish_core::data_structures::sorted_list::SortedList;` and
`...::sorted_collection::SortedCollection;`. Neither path exists at the public surface: `SortedList`
is re-exported as `data_structures::SortedList` (real module `data_structures::sorted::sorted_list`),
and `SortedCollection` is defined in the `pub(crate)` module `internal::sorted_collection` and
re-exported only as `data_structures::SortedCollection`. The canonical path used in
starfish-crossbeam is `starfish_core::data_structures::{SortedList, SortedCollection}`.

**Fix:** Change both examples to `use starfish_core::data_structures::{SortedList, SortedCollection};`
(and SkipList/MapCollection analogously) to match the actual re-export surface.

### DOC-4 (Low, docs): `SplitOrderedHashMap` parent-child hierarchy doc table contradicts the (correct) `get_parent_bucket` implementation

**File:** `src/data_structures/hash/split_ordered_hash_map.rs:169-176` vs :340-348

The module doc block lists parents as 3->2, 5->4, 6->4, 7->6 (clear lowest set bit). The code clears
the HIGHEST set bit: 3->1, 5->1, 6->2, 7->3, which matches the inline comment at :340 and the
Shalev-Shavit paper and is correct. Only the doc block at :169-176 is wrong, so a maintainer
trusting it could "fix" the code and break the structure.

**Fix:** Update the doc table at :169-176 to match the implementation (3->1, 5->1, 6->2, 7->3) and
change the explanation to "clear highest set bit".

### DOC-5 (Low, docs): `upsert.md` design doc has no status header and describes an unimplemented API as if it existed

**File:** `docs/upsert.md:1-12`

Per CLAUDE.md, every design doc must carry a Status/Crate/Key-files header; `upsert.md` has none. It
documents `upsert`, `upsert_from_internal`, `UpsertResult`, and a SortedList helper, but
`grep upsert starfish-core/src` returns no matches — none of these exist — so the doc reads as
shipped API when it is an unimplemented proposal.

**Fix:** Add a status header, e.g. `**Status:** NOT IMPLEMENTED` / `**Crate:** starfish-core` /
`**Key files:** src/data_structures/internal/sorted_collection.rs, src/data_structures/sorted/sorted_list.rs`.

### DOC-6 (Low, docs): `iterators-and-range-queries.md` is stale — status and struct/method designs do not match the shipped implementation

**File:** `docs/iterators-and-range-queries.md:39-51`

The doc's status is "PARTIAL" and it proposes `SkipListIter`/`SkipListRange`/`SkipListRef`/
`lower_bound`/`upper_bound` and per-SkipList `iter()`/`range()`. What shipped is different:
iteration/range are default methods on the `SortedCollection` trait via
`SortedCollectionIter`/`SortedCollectionRange`/`CollectionRef`, none of the proposed
SkipList-specific structs exist, and `IterableSortedCollection::iter_from` has zero implementors.

**Fix:** Either mark the doc `**Status:** SUPERSEDED` with a pointer to `sorted_collection.rs`, or
update the "Current State" section to describe the `SortedCollectionIter`/`SortedCollectionRange`/
`CollectionRef` implementation and note the SkipList-specific structs and `lower_bound`/
`upper_bound` were never built.

### DOC-7 (Low, docs): README `SortedCollectionInternal` code block omits the `type Guard` associated type the real trait declares

**File:** `README.md:81-92` vs `sorted_collection.rs:100-103`

The README block for `SortedCollectionInternal<T>` shows only `type Node` and `type NodePosition`,
but the real trait declares three associated types with `type Guard: Guard;` first. Since the prose
states the safe wrapper "pins the guard internally", omitting `Guard` misrepresents how the guard is
sourced.

**Fix:** Add `type Guard: Guard;` to the associated-type list in the README code block (:82).

### DOC-9 (Medium, docs): epoch-safety reframing not propagated to all remaining doc locations

**Files:** `src/data_structures/trie/skip_trie.rs:228-230` ("Iterators following UPDATE_MARK are
directed to new_node" — true for point reads only; iterators now *skip* the replacement) and the
`next_node` doc summary ("following UPDATE chains"); cross-crate:
`starfish-crossbeam/docs/benchmarks.md:15,27` (claims SkipList updates via `UPDATE_MARK` — its
map backends are value-CAS (`MapEntry`) and forward-splice node replacement (`Pair`)).
*(Resolved earlier: `map_entry.rs`, test headers, both READMEs incl. the Implementation Status
table.)*

**Fix:** apply the point-read-vs-iteration distinction at the skip_trie sites; correct
benchmarks.md.

### DOC-10 (Medium, docs): `SortedCollection::update` doc claim "replacing an element with an `Eq` element is observably a no-op" is false for key-only-`Eq` payloads

**Files:** `src/data_structures/internal/sorted_collection.rs:299-310`

`Pair<K,V>` and `MapEntry<K,V>` implement `Eq`/`Ord` by KEY ONLY, so two `Eq` elements can carry
observably different values. Safe external code calling `SortedCollection::update(Pair::new(k,
new_v))` on a map-typed collection gets `true` while the old value stays (for `MapEntry` the new
boxed value is silently dropped) — diverging from `MapCollection::update(k, new_v)` on the same
object.

**Fix:** state the limitation honestly: presence check, never modifies the stored element; for
partially-`Eq` payloads the carried value is NOT replaced — use `MapCollection::update`.

### DOC-11 (Medium, docs): `next_node_internal`'s trait-level contract not updated for the same-key-skip obligation the BUG-13 fix introduced

**Files:** `src/data_structures/internal/sorted_collection.rs:153-158`

The trait doc still reads "Get the next valid (unmarked) node after the given node", but the
duplicate-free guarantee is now load-bearing for the *generic* consumers in this file
(`SortedCollectionIter`, `len()`, `SortedCollectionRange` — which checks only the end bound and
would yield a below-start key if a predecessor's same-key replacement were returned). The
obligation lives only in per-impl comments; a future `NodeReplaceUpdate` implementor could miss
it. The effective non-sentinel-input requirement (BUG-24) belongs here too.

**Fix:** document on the trait method: must skip `node`'s same-key UPDATE replacement(s)
(duplicate-free iteration), and define the sentinel-input behavior chosen for BUG-24.

### TST-2 (Medium, tests): `test_concurrent_update_with_timeout` is a dead no-op masquerading as a hang regression test

**File:** `src/common_tests/sorted_collection_stress_tests.rs:760-795` (wired at `deferred_collection_tests.rs:140-146`)

The docstring claims it reproduces the benchmark hang and uses a timeout to detect hangs, but it
does nothing: `thread_count=1`, the only update call is commented out (:784), the cloned Arc is
bound to `_list_clone` and unused (:779), the loop bodies are empty (:782-786), there is no timeout
mechanism, and there are zero assertions. It cannot fail unless `join` panics, yet it is counted as
regression coverage for the concurrent-update hang path.

**Fix:** Either delete the test and its rstest case, or make it real: restore a contended
`thread_count`, uncomment the update call, run under an actual deadline/watchdog, and assert
progress/completion.

### TST-3 (Medium, tests): Several generic concurrent tests have no post-condition assertions (only verify no-panic/no-deadlock)

**File:** `src/common_tests/sorted_collection_core_tests.rs:99-145`; `sorted_collection_stress_tests.rs:386-444, 332-383, 447-483`

`test_concurrent_mixed_operations` (6 threads x 1000 mixed ops over 500 keys) returns after joining
with no assertion on final state, ordering, or count — a data-loss/lost-update/linearizability
corruption would pass silently as long as no thread panics. The same no-validation pattern appears
in `test_high_contention_mixed`, `test_concurrent_find_and_modify`, and `test_aba_problem`. The
bespoke SortedList tests already validate sorted/no-dup, so the generic versions are strictly weaker.

**Fix:** After joining, validate invariants the generic API supports: `to_vec()` strictly sorted
(`window[0] < window[1]`), no duplicate keys (HashSet), and `len() == to_vec().len()`.

### TST-4 (Low, tests): `test_stress_consistency_verification` uses a tautological assertion a fully-broken map would pass

**File:** `tests/split_ordered_hash_map_tests.rs:94-148` (assert at :140-147)

The remove+insert increment loop asserts only `actual <= expected` per key. Since lost updates only
make stored values smaller, this holds for ANY behavior including a map that loses every increment
(actual stays 0 <= expected). The test never establishes a lower bound, so it cannot detect
under-counting (the real CAS-loop failure mode) — only an impossible over-count.

**Fix:** Strengthen the invariant: track and assert a tighter relationship (conservation across
stored values plus lost-update accounting), or at minimum assert each key's final value is `> 0`
when its counter `> 0`, so an all-zeros map fails.

### TST-5 (Low, tests): Shared static LCG seed across parallel tests makes "random" stress tests non-deterministic and correlated

**File:** `tests/split_ordered_hash_map_tests.rs:461-475` (static `SEED` at :464)

The hand-rolled rand uses a single process-global `static SEED: AtomicUsize = AtomicUsize::new(12345)`
never reset per test. `test_stress_consistency_verification` and `test_stress_long_running_chaos`
draw from it concurrently (nextest runs tests in parallel), so each test's key sequence depends on
the other's interleaving — non-reproducible and correlated, and a failure cannot be reproduced from
a seed. The `fetch_add(1)` LCG also produces a fixed-stride sequence within a thread.

**Fix:** Give each test (and ideally each thread) its own seeded LCG state, or mark the rand-using
tests `#[serial]` and reset `SEED` at the start of each. Better, add a real dev-dependency PRNG
seeded deterministically per test.

### TST-6 (Medium, tests): `get_ref` has no `DeferredGuard` / core-crate test

**Files:** `get_ref` in `skip_list.rs` (`impl SkipList<MapEntry<K,V>, G>`); only callers are
`starfish-crossbeam/tests/skip_list_map_epoch.rs` (EpochGuard only).

The new public `get_ref` is exercised only under EpochGuard in the crossbeam stress test — no
deterministic core-crate test, no `DeferredGuard` coverage, and (without ASan) no UAF check.
**Fix:** add a `get_ref` test to `tests/map_collection_tests.rs` (DeferredGuard) asserting value,
post-update value, and `None`-on-absent.

*Status note (2026-06-09):* `test_map_tombstone_window_reads_absent` now covers
`get_ref`-on-tombstone under `DeferredGuard`; the positive-path assertions (value, post-update
value) are still missing.

### TST-7 (Low, tests): no self-checking leak/double-free assertion for replaced values

**Files:** `starfish-crossbeam/tests/skip_list_map_epoch.rs`.

The "no leaks" claim relies on ASan's at-exit report; the test asserts only presence / ordering /
`absent==0`. **Fix:** add a deterministic test with a `Drop`-counting `V` that asserts every
inserted/updated value is dropped exactly once — makes the leak/double-free claim self-checking
without ASan.

### TST-8 (Low, tests): EpochGuard test omits a held-`get_ref`-handle interleaving

**Files:** `starfish-crossbeam/tests/skip_list_map_epoch.rs`.

*(Narrowed 2026-06-09: part (a) — concurrent `remove` racing update + `get_ref` — is covered by
`epoch_map_update_remove_insert_churn`.)* Remaining gap: a `get_ref` handle *held* across many
updates of its key (the handle pins the epoch; the test should assert the borrowed value stays
valid and reclamation resumes after drop). **Fix:** add a held-handle reader thread.

### TST-10 (Medium, tests): node-replacement "validated epoch-safe" rests on update/read/iterate workloads only — no DELETE/INSERT racers, and UPDATE↔DELETE safety hinges on the gate BUG-11 proposes to relax

**Files:** `starfish-crossbeam/tests/sorted_list_map_epoch.rs`, `starfish-crossbeam/tests/skip_trie_map_epoch.rs`; `src/data_structures/trie/skip_trie.rs:1085`, :1407 (fully-linked gates)

Neither epoch test runs `remove()` or `insert()` against node-replacement UPDATE. Traced: SkipTrie
avoids a retire-while-relinkable zombie *only* because of the `succs[height] != curr` gate —
bottom-up linking means a deleter can never conclude "already unlinked at level L" while an
inserter's pred-link CAS is in flight toward the retired node. That gate is exactly what BUG-11
proposes to relax; a naive BUG-11 fix would reintroduce a real UAF and this suite would not catch
it (it never deletes). The epoch readers also never assert update-then-read value visibility
(values are `black_box`ed, not validated).

**Fix:** add remover+inserter threads to both epoch tests; put a SAFETY-grade comment at both
gate sites stating their epoch-safety role (so BUG-11's fix preserves it); scope the
"validated epoch-safe" doc claims to the tested workloads until then; assert read values are
∈ {expected old, expected new} for a tagged key. *(The SkipList map side of this gap is closed:
`skip_list_map_epoch.rs` now has `epoch_map_update_remove_insert_churn` — remove/insert racing
update/get_ref with quiescent accounting, ASan-clean 2026-06-09. SortedList/SkipTrie remain.)*

*Status note (2026-06-11):* `SkipTrie::unlink_at_level` now recovers off marked slots before any
"already unlinked" conclusion and advances through equal-key runs (the dead-pred frozen-slot fix
ported from SkipList), which removes one of the failure modes a naive BUG-11 fix could hit — but
the missing remover/inserter racers and gate-site SAFETY comments still stand.

### TST-11 (Medium, tests): no deterministic unit test for the iterator same-key skip (BUG-13 fix) — coverage is probabilistic, cross-crate, and slow

**Files:** `src/data_structures/sorted/sorted_list.rs` and `src/data_structures/trie/skip_trie.rs` `#[cfg(test)]` modules (absent coverage); `starfish-crossbeam/tests/{sorted_list,skip_trie}_map_epoch.rs` (only existing coverage)

`cargo test -p starfish-core` exercises the fix zero times. The 6s stress tests would reliably
catch a *full* regression of the predicate but can miss a *partial* one (e.g. handling the direct
replacement but not the update-during-update chain `X → X' → X''` the comments explicitly claim).
The mid-UPDATE state is trivially constructible deterministically: perform only step 4 of the
documented protocol (CAS `curr.next: succ → new_node|UPDATE_MARK`, skip the unlink), then assert
`first_node` + repeated `next_node` yield each key exactly once. Also fold in: 2-deep chain,
1-element list, minimum-key/`first_node` boundary, and BUG-24's below-min range start. (Minor:
the stress tests' single shared `fail: AtomicBool` makes failures undiagnosable — separate flags
or stash the first offending snapshot; the choke variants eprint but don't assert `final_len`.)

**Fix:** add the deterministic `#[cfg(test)]` tests in both files; improve stress-test failure
diagnostics.

*Status note (2026-06-10/11):* SkipList now HAS deterministic window tests
(`test_update_window_key_never_absent` covers the splice window incl. iteration no-duplicate;
`test_range_start_below_minimum_does_not_panic` covers the below-min range in both
sorted_list.rs and skip_trie.rs). SortedList/SkipTrie still lack the deterministic same-key-skip
and 2-deep-chain tests.

### TST-12 (Medium, tests): legacy "UPDATE" tests are vacuous since `SortedCollection::update` became a presence check — including a BUG-1 regression guard

**Files:** `src/data_structures/sorted/sorted_list.rs:1379-1559` (section "Update operation tests (UPDATE_MARK)": `test_update_basic`, `test_update_preserves_order`, `test_concurrent_updates`, `test_update_with_concurrent_delete/insert`); `src/data_structures/trie/skip_trie.rs:2736-2832` (`test_update_preserves_iteration_order`, `test_two_threads_same_key_update`, `test_concurrent_updates_same_keys` — commented as the BUG-1 regression guard)

Since commit `000eefe`, set-`update` only calls `find_from_internal` — these tests no longer touch
`NodeReplaceUpdate::update_internal`/`UPDATE_MARK` at all; they pass while testing nothing of the
protocol they were written for, and BUG-1 could regress undetected. Node-replacement is now only
reachable via `MapCollection::update` on `Pair` maps, and the deterministic map tests never
iterate concurrently with update.

**Fix:** port them to `SortedList<Pair>`/`SkipTrie<Pair>` via `MapCollection::update` (or delete
the meaningless ones and fix the misleading section headers).

### TST-13 (Low, tests): the "validated under ASan" claim has no automated backing

**Files:** `.github/workflows/ci.yml` (no sanitizer job); no script/justfile/Makefile/cargo alias with `-Zsanitizer` anywhere in the repo; claim baked into `starfish-core/README.md`, `src/data_structures/README.md`, `sorted_collection.rs`, `pair.rs`, this file, and both new test headers

Under plain `cargo test` a UAF regression in unlink-before-retire will very likely pass silently
(freed-but-unreclaimed epoch memory usually still reads plausibly), so the tests' primary stated
purpose only happens if someone remembers the incantation manually.

**Fix:** add a CI job or checked-in script:
`RUSTFLAGS=-Zsanitizer=address cargo +nightly test -p starfish-crossbeam --target x86_64-unknown-linux-gnu --test skip_list_map_epoch --test sorted_list_map_epoch --test skip_trie_map_epoch`;
at minimum record the exact command in each test file's `//!` header. Consider an env-tunable
test duration (ASan runs ~10× slower).

*Status note (2026-06-11):* the full validation-gate commands (ASan + release loops +
debug-assertions-release canary) are now recorded in `CLAUDE.md` ("Lock-free reclamation
validation gate"); CI automation is still absent.
