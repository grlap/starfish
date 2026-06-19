# MVCC via Flatten Skip List

Design for multi-version concurrency control using a single skip list with composite keys.

## Approach: Flatten (Composite Key)

Embed the version (transaction ID) directly into the skip list key. All versions of all
keys live in one skip list, ordered by `(key ASC, version DESC)`.

```
SkipList<MvccKey<K>, Option<V>>

(key=5, !txn=100) → Some(val_c)     ← newest version of key 5
(key=5, !txn=50)  → Some(val_b)
(key=5, !txn=10)  → Some(val_a)     ← oldest version of key 5
(key=7, !txn=80)  → Some(val_d)
(key=9, !txn=90)  → None            ← tombstone (key 9 deleted at txn 90)
(key=9, !txn=30)  → Some(val_e)     ← previous value before delete
```

Version is stored as bitwise NOT (`!txn_id`) so that **newest sorts first** within
each key using standard ascending `Ord`.

### Why flatten over nested?

| | Nested `SkipList<K, Chain<V>>` | Flatten `SkipList<(K, !ver), V>` |
|---|---|---|
| Get latest | O(log n) + chain head | O(log k) single seek |
| Get at version | O(log n) + chain walk | O(log k) single seek |
| Compaction | Cannot remove outer key (zombie) | True physical deletion |
| Implementation | Two data structures | Single skip list |
| Memory per version | Pointer overhead per chain node | Key bytes duplicated |

For point-lookup-dominant workloads with short-lived transactions, flatten wins on
simplicity, GC cleanliness, and implementation cost.

## Core Types

### MvccKey

```rust
/// Composite key for MVCC skip list.
/// Sorts by (key ASC, version DESC) — newest version first within each key.
#[derive(Clone, Debug)]
pub struct MvccKey<K> {
    pub key: K,
    /// Stored as !txn_id (bitwise NOT) so descending version = ascending sort.
    version: u64,
}

impl<K> MvccKey<K> {
    pub fn new(key: K, txn_id: u64) -> Self {
        Self { key, version: !txn_id }
    }

    pub fn txn_id(&self) -> u64 {
        !self.version
    }

    /// Construct a search key for "latest version of key".
    /// !u64::MAX = 0, sorts before all real versions.
    pub fn latest(key: K) -> Self {
        Self { key, version: 0 }  // !u64::MAX = 0
    }

    /// Construct a search key for "version at or before txn_id".
    pub fn at_version(key: K, txn_id: u64) -> Self {
        Self { key, version: !txn_id }
    }
}

impl<K: Ord> Ord for MvccKey<K> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
            .then(self.version.cmp(&other.version))
    }
}

impl<K: PartialEq> PartialEq for MvccKey<K> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.version == other.version
    }
}
impl<K: Eq> Eq for MvccKey<K> {}

impl<K: PartialOrd + PartialEq> PartialOrd for MvccKey<K> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.key.partial_cmp(&other.key)?
            .then(self.version.cmp(&other.version)))
    }
}
```

### MvccValue

```rust
/// Wrapper for values in the MVCC skip list.
/// `None` represents a tombstone (logical delete).
pub type MvccValue<V> = Option<V>;
```

### Snapshot

PostgreSQL-style snapshot. Transactions don't commit in order, so a single
watermark is insufficient — there can be "holes" where a higher txn_id has
committed while a lower one is still active.

```rust
/// Point-in-time snapshot for MVCC visibility checks.
/// Captured at transaction begin (repeatable read) or per-statement (read committed).
pub struct Snapshot {
    /// Oldest active txn_id at snapshot creation.
    /// All txn_ids < xmin are guaranteed committed → always visible.
    pub xmin: u64,
    /// First unassigned txn_id at snapshot creation.
    /// All txn_ids >= xmax are invisible (started after snapshot).
    pub xmax: u64,
    /// Highest committed txn_id at snapshot creation.
    /// Fast path: if txn_id <= max_committed and txn_id < xmin, skip active check.
    /// Also useful for seek optimization — start point reads at max_committed
    /// instead of xmax when active set is small.
    pub max_committed: u64,
    /// Txn_ids in [xmin, xmax) that were still active (uncommitted) at snapshot time.
    /// Sorted for binary search.
    pub active: Vec<u64>,
}

impl Snapshot {
    /// Is the given transaction's writes visible to this snapshot?
    pub fn is_visible(&self, txn_id: u64) -> bool {
        if txn_id < self.xmin { return true; }    // committed before snapshot
        if txn_id >= self.xmax { return false; }   // started after snapshot
        // In the gap [xmin, xmax): visible iff NOT in the active set
        self.active.binary_search(&txn_id).is_err()
    }
}
```

Commit status is tracked in a **commit log** (CLOG), not on skip list entries.
A single write to the CLOG makes an entire transaction's entries visible
atomically — no per-entry skip list updates needed.

**Read-your-own-writes**: the transaction's own `txn_id` is in `[xmin, xmax)`
and NOT in `active` (since the transaction knows its own writes are valid),
so `is_visible(own_txn_id)` returns `true` naturally.

### MvccEntry (full skip list entry type)

```rust
/// The type stored in the skip list.
/// SortedCollection<MvccEntry<K, V>> where MvccEntry: Ord
pub struct MvccEntry<K, V> {
    pub key: MvccKey<K>,
    pub value: MvccValue<V>,
}
```

Alternatively, if the skip list stores `(K, V)` tuples, the entry is
`(MvccKey<K>, Option<V>)`.

## Query Patterns

### Get latest committed version

```rust
fn get_latest(&self, key: &K) -> Option<&V> {
    // lower_bound finds first entry >= search key.
    // MvccKey::latest(key) has version=0 (!u64::MAX),
    // which sorts before all real versions of this key.
    let search = MvccKey::latest(key.clone());
    let entry = self.skip_list.lower_bound(&search)?;

    // Verify we landed on the right key (not the next key)
    if entry.key.key != *key {
        return None;
    }

    // Skip tombstones
    entry.value.as_ref()
}
```

### Get version at snapshot (snapshot read)

```rust
fn get_at_snapshot(&self, key: &K, snapshot: &Snapshot) -> Option<&V> {
    // Start search at xmax (first invisible txn). All versions >= xmax are
    // invisible, so lower_bound at (key, !xmax) skips past them.
    let search = MvccKey::at_version(key.clone(), snapshot.xmax);
    let mut cursor = self.skip_list.lower_bound(&search)?;

    // Walk forward through versions of this key, find newest visible
    loop {
        if cursor.key.key != *key {
            return None;  // past this key entirely
        }
        if snapshot.is_visible(cursor.key.txn_id()) {
            return cursor.value.as_ref();  // newest visible (tombstone = None)
        }
        cursor = self.skip_list.next(&cursor)?;  // try older version
    }
}
```

Note: unlike the simple `txn_id <= snapshot` check, `Snapshot::is_visible` must
handle holes (committed txns above active ones). The search starts at `xmax`
to skip definitely-invisible entries, then walks forward checking visibility
for each version in `[xmin, xmax)`.

### Complexity

```
k = n * m  (total entries = keys * avg versions per key)

Get latest:      O(log k) = O(log n + log m)
Get at version:  O(log k) = O(log n + log m)
Insert version:  O(log k)
Delete (tomb.):  O(log k)
Range scan:      O(log k + output)  (must skip old versions during scan)
```

The `log m` overhead is negligible in practice. With 1M keys x 1000 versions:
`log2(1B) ≈ 30` vs `log2(1M) ≈ 20`. Ten extra comparisons.

### Range scan (snapshot read)

Scan all visible keys in `[start, end)` at a given snapshot. The challenge: the
flatten layout interleaves all versions of all keys, so the iterator must:

1. Skip versions newer than the snapshot (invisible)
2. Yield only the newest visible version per key
3. Suppress keys whose newest visible version is a tombstone
4. Skip remaining old versions of each key before advancing

```
Scan [A, D) at snapshot = 75:

(A, !100) → Some(v3)   ← skip (txn 100 > 75, invisible)
(A, !50)  → Some(v2)   ← YIELD (newest visible for A)
(A, !10)  → Some(v1)   ← skip (older version of A)
(B, !80)  → None       ← skip (txn 80 > 75, invisible)
(B, !30)  → Some(v4)   ← YIELD (newest visible for B)
(C, !90)  → Some(v5)   ← skip (txn 90 > 75, invisible)
(C, !40)  → None       ← skip (tombstone — C deleted at snapshot 75)
(C, !10)  → Some(v6)   ← skip (tombstone hides this)
(D, ...)  → ...         ← stop (out of range)

Result: [(A, v2), (B, v4)]
```

#### MvccIter

Wraps the skip list's `SortedCollectionRange` iterator. The inner iterator is a
level-0 walk; MvccIter adds snapshot filtering on top.

```rust
pub struct MvccIter<'a, K, V, G: Guard> {
    /// Level-0 range iterator over Pair<MvccKey<K>, Option<V>>
    inner: SortedCollectionRange<'a, Pair<MvccKey<K>, Option<V>>, SkipList<...>, ...>,
    /// Point-in-time snapshot for visibility checks.
    snapshot: Snapshot,
    /// Key already yielded or tombstoned — skip remaining versions of this key.
    /// Set after finding the newest visible version for a key.
    skip_key: Option<K>,
}

impl<'a, K: Ord + Eq + Clone, V, G: Guard> Iterator for MvccIter<'a, K, V, G> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = self.inner.next()?;
            let pair: &Pair<MvccKey<K>, Option<V>> = entry.get();
            let mvcc_key = &pair.key;
            let user_key = &mvcc_key.key;

            // 1. Skip remaining versions of already-processed key
            if self.skip_key.as_ref() == Some(user_key) {
                continue;
            }

            // 2. Skip invisible versions
            if !self.snapshot.is_visible(mvcc_key.txn_id()) {
                continue;
            }

            // 3. Newest visible version for this key — mark as processed
            self.skip_key = Some(user_key.clone());

            // 4. Tombstone check
            match &pair.value {
                Some(v) => return Some((user_key, v)),
                None => continue,  // key deleted at this snapshot
            }
        }
    }
}
```

#### Skip strategy: linear vs tower resume

| Strategy | Available now? | Cost per key | Notes |
|---|---|---|---|
| **Linear skip** | Yes | O(m) | Walk `next()` until key changes. Simple. |
| **Tower resume** | No | O(log m) | Go up current node's tower until `fwd.key != current_key`, follow. Requires iterator to hold `SkipNodePosition` instead of bare `*mut Node`. |
| **Fresh seek** | Possible | O(log k) | New `range()` per key. Wasteful — re-seeks from head. |

**Current iterator state**: `SortedCollectionIter` / `SortedCollectionRange` hold a
single `current_node: Option<*mut Node>` (level-0 pointer). No tower state.

**`SkipNodePosition`** holds full 32-level predecessor arrays and is used internally
by `find_from_internal()` and batch insert — but is not exposed through iterators.

**Plan**: Start with linear skip. If profiling shows version-skip overhead in scans
with high version counts, add a `seek(&T)` method to the iterator backed by
`find_from_internal()`, upgrading iterator state to hold `SkipNodePosition`.

#### Open design questions — range queries

1. ~~**Peek vs consume**: `SortedCollectionRange` has no `peek()`.~~
   **Resolved: skip_key field.** Neither peek nor Peekable needed. MvccIter stores
   `skip_key: Option<K>` — the key already yielded or tombstoned. The main loop
   checks `skip_key` before processing each entry, skipping remaining versions
   implicitly. Requires `K: Clone` (one clone per distinct key yielded). The
   `skip_past_key()` method is eliminated — version skipping is folded into
   the main `next()` loop.

2. ~~**Read-your-own-writes**~~
   **Resolved: Snapshot + CLOG.** Writes are inserted into the skip list at write
   time with the transaction's `txn_id`. Visibility is controlled by `Snapshot`
   (xmin/xmax/active), not by a per-entry commit flag. The transaction's own
   `txn_id` is excluded from `snapshot.active`, so `is_visible(own_txn_id)` returns
   `true` — own writes are visible naturally. No merge iterator needed. Commit is
   a single CLOG write (atomic). Abort removes the transaction's entries from the
   skip list. Using the skip list's `update` to stamp a committed flag was
   considered but rejected: O(writes) CAS updates, non-atomic visibility window.

3. ~~**Predicate pushdown**~~
   **Resolved: standard iterator composition.** MvccIter yields `(&K, &V)`
   references — no materialization cost. Callers use `.filter()`:
   `mvcc_iter.filter(|(k, v)| v.amount > 100)`. No special pushdown API
   needed for in-memory skip list. Revisit when adding LSM/page-backed
   storage where value fetch has real I/O cost.

4. ~~**Reverse iteration**~~
   **Deferred.** Skip list is singly-linked at level 0. Reverse scan requires
   backlinks or a separate reverse-order index. Not practical with current
   data structure. Revisit if workloads require it.

#### Epoch pinning strategy: repin per element

A long-running range scan should not hold back GC for its entire lifetime.
Instead, the iterator repins the epoch after each yielded element, allowing
the global epoch to advance and compaction to reclaim memory.

crossbeam-epoch provides `Guard::repin()`. The `Guard` trait in starfish-core
needs the same:

```rust
// Addition to Guard trait (starfish-core/src/guard/mod.rs)
pub trait Guard: Sized + Default + Send + Sync {
    type ReadGuard: Sized;
    fn pin() -> Self::ReadGuard;
    // ...existing methods...

    /// Advance the thread's local epoch to the current global epoch.
    /// The thread remains pinned — there is no gap in protection.
    /// This allows older epochs to be reclaimed by GC.
    fn repin(guard: &mut Self::ReadGuard);
}

// EpochGuard impl (wraps crossbeam):
impl Guard for EpochGuard {
    fn repin(guard: &mut CrossbeamGuard) {
        guard.repin();  // crossbeam native
    }
}

// DeferredGuard impl (testing):
impl Guard for DeferredGuard {
    fn repin(_guard: &mut ()) {}  // no-op
}

// Future SingleThreadGuard (thread-per-core):
impl Guard for SingleThreadGuard {
    fn repin(_guard: &mut ()) {}  // no-op, single-threaded
}
```

**Why this is safe**: `repin()` atomically sets the thread's local epoch to the
current global epoch — the thread is never unpinned. There is no gap in
protection. Pointers loaded before repin remain valid. What changes is that
the GC can now reclaim objects deferred in older epochs (because this thread
is no longer holding back those epochs). Objects deferred in epoch E are only
freed when all threads have advanced past E, so the current node pointer
(loaded in a recent epoch) cannot be freed by a single repin.

**MvccIter with repin**:

```rust
fn next(&mut self) -> Option<Self::Item> {
    loop {
        let entry = self.inner.next()?;
        // ...skip_key check, snapshot filter...
        self.skip_key = Some(user_key.clone());
        match &pair.value {
            Some(v) => {
                // Repin after yielding — allow GC progress
                G::repin(&mut self.inner.guard);
                return Some((user_key, v));
            }
            None => continue,
        }
    }
}
```

The iterator holds the epoch for exactly one element at a time. Between
`next()` calls by the consumer, the epoch is re-pinned and GC can progress.

#### Non-issue: iterator invalidation under compaction

Not a concern, even with repin. The epoch guarantee ensures that unlinked
nodes retain valid level-0 forward pointers until reclaimed. Any compacted
entries the iterator encounters are old versions that the snapshot filter
skips anyway (`txn_id < min_active_snapshot ≤ iterator's snapshot`).

## Write Operations

### Insert (new version)

```rust
fn put(&self, txn_id: u64, key: K, value: V) {
    let mvcc_key = MvccKey::new(key, txn_id);
    self.skip_list.insert((mvcc_key, Some(value)));
}
```

Standard skip list insert. Each transaction creates new nodes — no mutation of
existing entries. Readers of older snapshots see older versions undisturbed.

### Delete (tombstone)

```rust
fn delete(&self, txn_id: u64, key: K) {
    let mvcc_key = MvccKey::new(key, txn_id);
    self.skip_list.insert((mvcc_key, None));  // tombstone
}
```

A tombstone is a real skip list entry with `value = None`. When a snapshot reader
encounters a tombstone as the newest visible version, the key does not exist in
that snapshot.

### Update

```rust
fn update(&self, txn_id: u64, key: K, value: V) {
    // In flatten MVCC, update = insert new version.
    // Old version remains for older snapshots.
    self.put(txn_id, key, value);
}
```

No need for the atomic UPDATE_MARK here — each version is a separate node.
The old version is never mutated or removed (until compaction).

## Compaction (Garbage Collection)

### When to compact

Compaction removes versions that are no longer visible to any active snapshot.

```
min_active_snapshot = min(all active transaction snapshot LSNs)
```

Any version with `txn_id < min_active_snapshot` AND a newer committed version
exists for the same key can be physically deleted.

### Algorithm

```rust
fn compact(&self, min_active_snapshot: u64) {
    let mut iter = self.skip_list.iter();
    let mut prev_key: Option<K> = None;
    let mut seen_live_version = false;

    while let Some(entry) = iter.next() {
        let txn_id = entry.key.txn_id();
        let same_key = prev_key.as_ref() == Some(&entry.key.key);

        if same_key && seen_live_version && txn_id < min_active_snapshot {
            // This version is:
            //   - not the newest for this key (seen_live_version = true)
            //   - older than all active snapshots
            // Safe to remove.
            self.skip_list.remove(&entry.key);
        }

        if !same_key {
            prev_key = Some(entry.key.key.clone());
            seen_live_version = false;
        }

        if txn_id >= min_active_snapshot || !seen_live_version {
            // This version might still be needed
            seen_live_version = true;
        }
    }
}
```

### Tombstone cleanup

A tombstone `(key, !txn, None)` can be removed when:
1. `txn < min_active_snapshot` (no reader can see it)
2. No older versions of the same key exist (nothing to hide)

After removing all older versions, the tombstone itself becomes eligible.

### What gets deleted

```
Before compaction (min_active_snapshot = 60):

(5, !100) → Some(val_c)     ← keep (newest, above threshold)
(5, !50)  → Some(val_b)     ← DELETE (older, superseded, below threshold)
(5, !10)  → Some(val_a)     ← DELETE (older, superseded, below threshold)
(7, !80)  → Some(val_d)     ← keep (only version)

After compaction:

(5, !100) → Some(val_c)
(7, !80)  → Some(val_d)
```

True physical deletion. No zombie keys. The skip list shrinks.

## Integration with starfish-storage

### Where MVCC sits in the stack

```
┌─────────────────────────────────────────────────────────────┐
│                   Transaction Layer                         │
│  begin(isolation) → txn_id (monotonic u64)                  │
│  commit / abort                                             │
│  tracks active snapshots for GC                             │
├─────────────────────────────────────────────────────────────┤
│                     MVCC Index                              │
│  SkipList<MvccKey<K>, Option<V>>                            │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  (key, !txn_id) → value                             │    │
│  │  lower_bound for point reads                        │    │
│  │  iter + skip for range scans                        │    │
│  │  compaction thread removes old versions             │    │
│  └─────────────────────────────────────────────────────┘    │
├─────────────────────────────────────────────────────────────┤
│                     Buffer Pool / Pages                     │
│  For large values: skip list stores (key, !txn) → PageId    │
│  Actual record data lives in slotted pages                  │
├─────────────────────────────────────────────────────────────┤
│                         WAL                                 │
│  All writes logged before visible                           │
│  Recovery replays WAL to rebuild skip list                  │
└─────────────────────────────────────────────────────────────┘
```

### Transaction lifecycle

```rust
// 1. Begin — allocate monotonic txn_id, register snapshot
let txn_id = self.next_txn_id.fetch_add(1, Ordering::SeqCst);
let snapshot = self.min_committed_txn(); // or specific isolation logic
active_snapshots.insert(txn_id, snapshot);

// 2. Reads — use snapshot as version bound
let val = mvcc_index.get_at_version(&key, snapshot);

// 3. Writes — buffer in write set, apply on commit
write_set.push((key, value));

// 4. Commit
//    a. WAL: write all entries
//    b. Apply: insert into skip list with txn_id
//    c. Remove from active_snapshots
for (key, value) in write_set {
    wal.append(WalEntry::Insert { txn_id, key, value });
    mvcc_index.put(txn_id, key, value);
}
active_snapshots.remove(&txn_id);

// 5. Background: compaction thread
let min_snapshot = active_snapshots.values().min();
mvcc_index.compact(min_snapshot);
```

### Isolation levels

| Level | Snapshot behavior |
|---|---|
| Read Uncommitted | No snapshot — see uncommitted writes (read latest) |
| Read Committed | Snapshot refreshed on each read |
| Repeatable Read | Snapshot fixed at transaction begin |
| Serializable | Snapshot fixed + write conflict detection at commit |

All levels use the same `get_at_version` / `lower_bound` mechanism.
Only the snapshot LSN management differs.

### Write conflict detection (Serializable)

At commit time, check if any key in the write set was modified by another
committed transaction since our snapshot:

```rust
fn check_conflicts(&self, txn: &Transaction) -> Result<()> {
    for key in &txn.write_set {
        let latest = self.mvcc_index.get_latest(key);
        if let Some(entry) = latest {
            if entry.txn_id() > txn.snapshot && entry.txn_id() != txn.id {
                return Err(SerializationError::WriteConflict);
            }
        }
    }
    Ok(())
}
```

## LSM Tree Integration

For large datasets backed by LSM trees, the flatten model maps naturally:

- **MemTable**: `SkipList<MvccKey<K>, Option<V>>` — same composite key
- **SSTable**: entries sorted by `(key, !version)` — same ordering on disk
- **Compaction**: SSTable merge drops versions below `min_active_snapshot`
- **Bloom filter**: keyed on `K` only (not version), so point lookups check
  bloom on the user key and then binary search for the right version

```
Write path:
  put(txn_id, key, val) → MemTable insert → WAL

Flush:
  MemTable → SSTable (sorted by MvccKey, preserving version order)

Compaction:
  Merge SSTables, drop old versions below min_active_snapshot
  Drop tombstones when no older versions remain
```

FIFO compaction for streams still works — stream entries are append-only with
monotonically increasing keys, so version ordering = insertion ordering.

## Threading Model: Shared-Everything vs Thread-Per-Core

The MVCC skip list must work under two deployment modes, driven by
`starfish-reactor`'s architecture.

### Mode A: Shared-Everything (lock-free)

Classic multi-threaded model. All threads access all data concurrently.

```
┌──────────────────────────────────────────────────────────────┐
│                    Shared MVCC Index                          │
│         SkipList<MvccKey<K>, Option<V>>                       │
│         (lock-free, CAS-based, epoch GC)                     │
│                                                              │
│   Thread 0 ──┐                                               │
│   Thread 1 ──┼──► concurrent insert / lower_bound / compact  │
│   Thread 2 ──┤                                               │
│   Thread N ──┘                                               │
└──────────────────────────────────────────────────────────────┘
```

- Uses `SkipListBacklinks` with `EpochGuard` (existing starfish-core)
- CAS on every insert, atomic pointer reads on every lookup
- Epoch-based reclamation for safe node deallocation
- Good for: few cores, high cross-key locality, simple deployment

### Mode B: Thread-Per-Core (shared-nothing, sharded)

Each reactor core owns a partition of the keyspace. No concurrent access to
the same skip list instance — single-threaded hot path, cross-shard via
message passing.

```
┌────────────┐  ┌────────────┐  ┌────────────┐  ┌────────────┐
│   Core 0   │  │   Core 1   │  │   Core 2   │  │   Core N   │
│            │  │            │  │            │  │            │
│ Shard[0]   │  │ Shard[1]   │  │ Shard[2]   │  │ Shard[N]   │
│ SkipList   │  │ SkipList   │  │ SkipList   │  │ SkipList   │
│ WAL segment│  │ WAL segment│  │ WAL segment│  │ WAL segment│
│ Buffer Pool│  │ Buffer Pool│  │ Buffer Pool│  │ Buffer Pool│
│            │  │            │  │            │  │            │
│  (no CAS   │  │  (no CAS   │  │  (no CAS   │  │  (no CAS   │
│   needed)  │  │   needed)  │  │   needed)  │  │   needed)  │
└─────┬──────┘  └─────┬──────┘  └─────┬──────┘  └─────┬──────┘
      │               │               │               │
      └───────────────┴───────┬───────┴───────────────┘
                              │
                    message passing for
                    cross-shard transactions
```

**Why this matters for MVCC:**

1. **No CAS overhead**: Each skip list is single-owner. Insert is a plain
   pointer write, not a CAS loop. No epoch pinning needed for reads.
   This can be 2-5x faster than the lock-free path on the hot path.

2. **No epoch GC complexity**: Single-threaded access means nodes can be
   freed immediately (or batched by the owning core). No global epoch
   coordination.

3. **Per-core WAL**: Each core appends to its own WAL segment. No contention
   on the WAL writer. Flush/sync can be batched per-core.

4. **Per-core compaction**: Each core compacts its own shard's skip list.
   No cross-core coordination except for global `min_active_snapshot`.

5. **Per-core buffer pool**: Page cache is core-local. No false sharing
   or cache-line bouncing.

### Sharding strategy

Partition keys across cores:

```rust
fn shard_for_key<K: Hash>(key: &K, num_cores: usize) -> usize {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish() as usize % num_cores
}
```

Or range-based sharding for range-scan-heavy workloads:

```rust
fn shard_for_key_range<K: Ord>(key: &K, boundaries: &[K]) -> usize {
    boundaries.partition_point(|b| b <= key)
}
```

### Cross-shard transactions

When a transaction touches keys on multiple cores:

```
Core 0 (coordinator)            Core 2 (participant)
  │                               │
  │  ── Prepare(txn, writes) ──►  │
  │                               │  validate + lock keys
  │  ◄── PrepareOk ────────────   │
  │                               │
  │  ── Commit(txn_id) ────────►  │
  │                               │  apply to local skip list
  │  ◄── CommitOk ─────────────   │
```

1. **Single-shard transactions** (common case): execute entirely on the
   owning core. No coordination. No message passing. Fast path.

2. **Cross-shard transactions**: 2PC via reactor message passing.
   Coordinator core drives prepare/commit across participant cores.
   Each participant validates and applies locally.

3. **Snapshot reads**: `min_active_snapshot` is a single atomic u64
   readable from any core (or propagated periodically via messages).

### Skip list variant for thread-per-core

The lock-free `SkipListBacklinks<T, EpochGuard>` works in thread-per-core
mode — it's just paying for atomics it doesn't need. Two options:

**Option A: Reuse lock-free skip list as-is.**
Simpler. CAS on an uncontended cache line is ~5-10ns overhead. The atomics
cost is measurable but small relative to the work per operation. This avoids
maintaining two skip list implementations.

**Option B: Add a `SingleThreadGuard` (no-op guard).**
Parameterize the skip list over a guard that skips epoch pinning and uses
plain pointer writes instead of CAS:

```rust
/// No-op guard for single-threaded (thread-per-core) use.
/// Pointers are plain writes. No epoch, no CAS.
pub struct SingleThreadGuard;

impl Guard for SingleThreadGuard {
    type GuardedRef<'a, T> = &'a T;
    type ReadGuard = ();

    fn pin() -> () { }  // no-op

    unsafe fn defer_destroy<N>(&self, node: *mut N, dealloc: unsafe fn(*mut N)) {
        // Immediate deallocation — no other thread can hold a reference.
        unsafe { dealloc(node); }
    }
}
```

The skip list's `SortedCollection` trait is already generic over `Guard`.
A `SingleThreadGuard` would turn every CAS into a plain store and every
epoch pin into a no-op — zero overhead. Compaction frees memory immediately
rather than deferring to epoch advancement.

**Recommendation: Start with Option A** (reuse existing lock-free skip list).
Profile. If atomic overhead shows up in flame graphs on the per-core hot path,
add `SingleThreadGuard` as Option B. The trait-generic design makes this a
non-breaking addition.

### Hybrid: per-core indexes + shared metadata

```
┌─────────────────────────────────────────────────────┐
│              Shared (atomic / lock-free)            │
│  ┌──────────────────────────────────────────────┐   │
│  │  TxnIdAllocator: AtomicU64                   │   │
│  │  MinActiveSnapshot: AtomicU64                │   │
│  │  CommittedTxnWatermark: AtomicU64            │   │
│  └──────────────────────────────────────────────┘   │
├─────────────────────────────────────────────────────┤
│            Per-Core (single-threaded)               │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐    │
│  │  Core 0     │ │  Core 1     │ │  Core N     │    │
│  │  MVCC Index │ │  MVCC Index │ │  MVCC Index │    │
│  │  WAL seg.   │ │  WAL seg.   │ │  WAL seg.   │    │
│  │  Buf. Pool  │ │  Buf. Pool  │ │  Buf. Pool  │    │
│  │  Active txns│ │  Active txns│ │  Active txns│    │
│  └─────────────┘ └─────────────┘ └─────────────┘    │
└─────────────────────────────────────────────────────┘
```

Only a few atomic u64s are truly shared. Everything else is core-local.
This gives ScyllaDB/Redpanda-level scalability while keeping the MVCC
model simple.

## Memory Considerations

### Key duplication

Each version duplicates the key bytes. Mitigation strategies:

1. **Intern keys**: For `Vec<u8>` keys, use `Arc<[u8]>` — versions share the
   key allocation via reference counting.
2. **Small keys**: For integer or fixed-size keys, duplication is negligible.
3. **Aggressive compaction**: Keep version count low per key.

### Skip list overhead

Each node in the skip list has a pointer tower (avg 2 pointers at p=0.5).
With k = n*m entries, total pointer overhead = k * 2 * 8 bytes.

For 1M keys x 10 versions = 10M entries: ~160MB in pointers alone.
Compaction keeps this bounded by removing old versions.

## References

- [crossbeam-skiplist-mvcc](https://github.com/al8n/crossbeam-skiplist-mvcc) — flatten and nested approaches
- [Erta: Exploring the Design Space of MVCC](https://www.vldb.org/pvldb/vol16/p1).
- [An Empirical Evaluation of In-Memory MVCC](https://db.in.tum.de/~muehlbau/papers/mvcc.pdf)
- [Hekaton: SQL Server's Memory-Optimized OLTP Engine](https://www.microsoft.com/en-us/research/publication/hekaton-sql-servers-memory-optimized-oltp-engine/)

## Issues

Review findings against `starfish-core/src/data_structures/sorted/skip_list.rs`:

1. **`lower_bound` is used throughout this doc, but the current public API does not expose it.**
   - `SortedCollection` currently exposes `find`, `range`, and `iter`.
   - MVCC examples using `self.skip_list.lower_bound(...)` will not compile as written.

2. **Make the concrete MVCC representation explicit for `starfish-core` integration.**
   - Planned concrete shape: `SkipList<Pair<MvccKey<K>, Option<V>>, G>`.
   - This preserves the flatten ordering by composite key because `Pair` orders by its key field (`MvccKey<K>`).
   - If using a custom entry struct instead of `Pair`, it must implement `Eq/Ord/Borrow` keyed on `MvccKey<K>`.

3. **Backlinks references are stale.**
   - This doc mentions `SkipListBacklinks`, but current implementation explicitly uses predecessor recovery from higher levels and avoids backlinks.

4. **Compaction lifecycle snippet has a type mismatch.**
   - `compact` is defined with `min_active_snapshot: u64`.
   - The call site uses `active_snapshots.values().min()` which is an `Option`.
   - The empty-active-set behavior needs to be defined (for example, map `None` to `u64::MAX`).

5. **Serializable conflict snippet is inconsistent with `get_latest` return type.**
   - `get_latest` is shown as `Option<&V>`.
   - Conflict detection later expects `entry.txn_id()`, which requires metadata beyond `&V`.
   - Either return an MVCC entry wrapper from `get_latest` or add a separate metadata lookup.

6. **Implementation caveat in current `skip_list.rs`: map `len()` can drift if APIs are mixed.**
   - For `SkipList<Pair<K, V>, G>`, `MapCollection::len` relies on `size`.
   - `size` is adjusted by map insert/remove paths, not by generic `SortedCollection` insert/delete paths.
   - If both APIs are used on the same instance, observed map length can be inaccurate.
