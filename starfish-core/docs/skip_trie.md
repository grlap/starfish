# SkipTrie Performance Analysis

## Reference

Oshman & Shavit, "The SkipTrie: Low-Depth Concurrent Search without Rebalancing", PODC 2013.

The SkipTrie is a probabilistically-balanced y-fast trie: a truncated skiplist (height = log log u)
whose top-level nodes are inserted into an x-fast trie. The original paper uses a split-ordered
hash map (Shalev & Shavit, 2003); our implementation replaces it with a PrefixTable
(open-addressing with linear probing) for better cache locality.

## Conclusion

**The SkipTrie does not scale in practice.** The theoretical O(log log u) bounds are correct,
but the constant factors from hash map cache misses make it slower than a plain skiplist at
every tested scale. The ratio worsens superlinearly: 4x at 1K keys, 47x at 200K, 219x at 500K.

The paper's analysis treats the split-ordered hash table as an O(1) atomic object (Section 4.2),
which is valid asymptotically but not in wall-clock time. In practice, each hash operation costs
5ns when the table fits in L1 cache vs 200-400ns when it spills to main memory. This difference
alone negates the SkipTrie's O(log log u) vs O(log n) depth advantage.

## Theoretical Complexity

| Operation    | Amortized Expected | Notes |
|--------------|-------------------|-------|
| Predecessor  | O(log log u + c)  | c = overlapping-interval contention |
| Insert       | O(c log log u)    | Top-level insert costs O(log u) but happens with P = 1/log u |
| Delete       | O(c log log u)    | Same amortization as insert |
| Space        | O(m)              | m = number of keys |

## Architecture

```
User operation
  -> find_lowest_ancestor()     [O(log log u) hash map lookups]
  -> skiplist traversal          [O(log log u) levels, ~2 steps/level]
  -> if top-level: insert_node() [O(log u) hash map inserts, amortized O(1)]
```

### Key parameters

| Parameter | Formula | Example (u = 2^19, N = 500K) |
|-----------|---------|------------------------------|
| key_bits  | ceil_log2(universe_size) | 19 |
| height    | ceil(log2(log2(u))) | 5 |
| P(top)    | 1/2^height | 1/32 |
| Prefixes per top-level node | key_bits + 1 | 20 |
| find_lowest_ancestor lookups | ~log2(key_bits) | ~5 |

## Implementation Status

### Fixed Bugs

- **BUG-3**: insert_node/remove_node used plain `store` instead of CAS for trie pointer updates.
  Fixed with CAS loops (insert) and single CAS (remove).
- **BUG-4**: key_to_hash used DefaultHasher (non-order-preserving). Fixed with `KeyBits` trait
  providing order-preserving bit encoding. Uses bottom-bits masking: `bits & ((1 << key_bits) - 1)`.


## Benchmark Results (M2, after all optimizations)

### Batch Insert Scaling (single-thread, sequential keys 0..N)

| N | SkipList | SkipTrie | Ratio | SkipTrie/N (ns/key) |
|---|----------|----------|-------|---------------------|
| 1K | 57µs | 226µs | 4.0x | 226 |
| 10K | 554µs | 4.67ms | 8.4x | 467 |
| 100K | 5.8ms | 129ms | 22x | 1290 |
| 200K | 12.6ms | 591ms | 47x | 2955 |
| 500K | 32.9ms | ~7.2s | 219x | 14400 |

### Per-key cost growth

SkipList: ~57-66 ns/key (roughly constant — O(log n) with good cache locality)
SkipTrie: 226 -> 14400 ns/key (grows ~64x over 500x size increase)

### Multithreaded (1000 keys, insert-only)

| Threads | SkipList | SkipTrie | Ratio |
|---------|----------|----------|-------|
| 1 | 912µs | 3.72ms | 4.1x |
| 4 | 1.60ms | 11.5ms | 7.2x |
| 16 | 3.99ms | 52.3ms | 13.1x |

### Contention (100 keys, insert+delete)

| Threads | SkipList | SkipTrie | Ratio |
|---------|----------|----------|-------|
| 1 | 396µs | 1.23ms | 3.1x |
| 16 | 847µs | 1.96ms | 2.3x |

Contention workload shows the **best** SkipTrie ratios because the hash map stays small (100 keys)
and fits entirely in cache.

## Why It Doesn't Scale

### The chain traversal is not the problem

The split-ordered hash map's chain traversal stays bounded at ~3-5 nodes per lookup
(load factor ≤ MAX_LOAD = 4, capped at ~4.8 when MAX_BUCKETS is reached). This is constant.

The amortized hash operations per insert are also roughly constant (~5-6 ops).

### The problem: per-node cache miss cost

Each SortedListNode is ~80 bytes, individually heap-allocated (`Box::new`), scattered in memory.
The cost per node access depends entirely on which cache level it hits:

| N | Hash map entries | Hash map memory | Cache tier | Per-node cost |
|---|-----------------|----------------|-----------|---------------|
| 1K | ~680 | 55 KB | L1/L2 | ~5 ns |
| 10K | ~9.4K | 750 KB | L2 | ~15 ns |
| 100K | ~56K | 4.5 MB | L3 | ~50-100 ns |
| 200K | ~119K | 9.5 MB | L3 edge | ~100-200 ns |
| 500K | ~97K unique | ~10 MB | Main memory | ~200-400 ns |

Note: at 500K, prefix sharing reduces entries from 312K attempts to ~97K unique, but this
still far exceeds L2 cache.

### Hash map entry count breakdown (N = 500K)

Top-level nodes: ~15,625 (1/32 of N). Each inserts 20 prefix levels (key_bits + 1).
Total insert attempts: 312,500. Unique entries stored: ~97K (shorter prefixes are shared).
Bucket sentinels: ~32K. Total linked list nodes: ~129K.

Total hash map operations for 500K inserts: ~2.8M (2.5M find_lowest_ancestor + 312K insert_node).

### The fundamental mismatch

The SkipTrie trades O(log n) skiplist comparisons for O(log log u) hash map lookups.
For this to be a win, each hash lookup must be cheaper than the ~14 skiplist comparisons saved:

```
Skiplist saving:  ~14 comparisons * ~10 ns = ~140 ns saved
Hash map cost:    ~5 lookups * per-lookup-cost

At 1K entries:    5 * 5ns   =  25ns  -> SkipTrie wins (saves 115ns)
At 100K entries:  5 * 100ns = 500ns  -> SkipTrie loses (costs 360ns extra)
At 500K entries:  5 * 400ns = 2000ns -> SkipTrie loses badly (costs 1860ns extra)
```

The crossover is around **50-100K entries** — the point where the hash map exceeds L2 cache.
Beyond that, every hash lookup is a cache miss, and the SkipTrie's theoretical depth advantage
is overwhelmed by a ~100x difference in per-operation wall-clock cost.

### Why the paper's analysis doesn't predict this

The paper (Section 4.2) models the hash table as an **O(1) atomic object** — each operation
takes unit cost regardless of table size. This is standard for theoretical analysis but hides
the cache hierarchy entirely. In practice:

- O(1) at 1K entries = 5ns (L1 cache hit)
- O(1) at 500K entries = 400ns (main memory miss)

The 80x difference in "O(1)" is the entire story. No algorithmic optimization can fix this
while using a pointer-chased linked list as the hash map's backing store.

Arena allocation could reduce the constant factor (contiguous nodes = fewer cache misses),
but the fundamental scaling problem remains: the hash map's working set grows with N,
while a skiplist's per-operation working set is O(log n) nodes along a single search path.

---

## Implementation Plan: Making the SkipTrie Practical

Two changes target the two root causes: (1) each hash lookup is slow due to pointer-chasing
cache misses, and (2) find_lowest_ancestor does ~5 hash lookups when most return "not found".

### Phase 1: PrefixTable — Open-Addressing Hash Table

Replace `SplitOrderedHashMap<u64, XFastTrieNode<T>, G>` with a flat open-addressing table
where slots are contiguous in memory. Linear probing touches adjacent cache lines, so the
hardware prefetcher eliminates most cache misses within a probe sequence.

#### Slot layout

```rust
const EMPTY_KEY: u64 = 0;  // Safe: encode_prefix always returns >= 1

#[repr(C)]
struct PrefixSlot<T> {
    key: AtomicU64,                            // 8 bytes
    pointers: [AtomicPtr<SkipTrieNode<T>>; 2], // 16 bytes
}
// 24 bytes per slot, ~2.6 slots per cache line
```

#### Design decisions

- **No tombstones**: Insert-only table. When `remove_node` nulls out both pointers, the
  slot stays with its key — ready for reuse by the next top-level node sharing that prefix.
  The `remove_internal` call becomes a no-op.
- **Empty = key 0**: `encode_prefix` always sets a sentinel bit, so valid keys are ≥ 1.
- **Linear probing**: Power-of-2 table size, mask = capacity - 1. Hash with FxHash (single
  multiply + shift for u64 keys).
- **Grow by rehash**: Start at 2^16 = 65K slots (1.5 MB). When count > 60% capacity,
  allocate 2x table and rehash. Readers check both tables during migration.

#### API (replaces 4 HashMapCollection methods)

```rust
impl<T> PrefixTable<T> {
    /// Find a slot by prefix key. Returns reference to slot if found.
    fn find(&self, prefix_key: u64) -> Option<&PrefixSlot<T>>;

    /// Find existing slot or insert a new one. Always succeeds.
    /// Concurrent inserts of the same key converge to one slot.
    fn find_or_insert(&self, prefix_key: u64) -> &PrefixSlot<T>;
}
```

#### Changes to XFastTrie

```
Before:  prefixes: SplitOrderedHashMap<u64, XFastTrieNode<T>, G>
After:   prefixes: PrefixTable<T>
```

Method changes:
- `find_lowest_ancestor`: `find_internal` + `apply_on_internal` → `prefixes.find(prefix_key)`
  then read `slot.pointers[direction]` directly.
- `insert_node`: `find_internal` / `insert_internal` / `apply_on_internal` →
  `prefixes.find_or_insert(prefix_key)` then CAS on `slot.pointers[direction]`.
- `remove_node`: `find_internal` / `apply_on_internal` / `remove_internal` →
  `prefixes.find(prefix_key)` then CAS pointer to null. No slot removal.

The Guard parameter `G` is no longer needed by XFastTrie (no epoch-based reclamation for
flat array slots). Remove `G` from `XFastTrie<T, G>` → `XFastTrie<T>`.

#### Expected impact

Per-lookup cost at 500K entries: ~100-150 ns (1 cache miss + ~1.5 sequential probes)
vs current ~400 ns (3-5 pointer chases into random memory).

```
find_lowest_ancestor:  5 * 120ns = 600ns  (was 2000ns)  → 3.3x faster
insert_node (per top): 20 * 120ns = 2.4µs (was 8µs)    → 3.3x faster
Total 500K insert:     ~2.2s (was ~7.2s)                 → ~3.3x faster
Ratio vs skiplist:     ~67x (was 219x)
```

### Phase 2: PrefixBitmap — Per-Level Existence Check

Add a bitmap per trie level so find_lowest_ancestor can skip hash lookups for
non-existent prefixes. The binary search typically hits ~2 existing prefixes
out of ~5 probes — the bitmap eliminates the 3 misses entirely.

#### Layout

```rust
struct PrefixBitmap {
    /// levels[L] has ceil(2^L / 64) AtomicU64 words.
    /// Bit j in word w represents raw prefix (w * 64 + j) at level L.
    levels: Vec<Box<[AtomicU64]>>,
    key_bits: usize,
}
```

Memory for key_bits = 19: sum(ceil(2^i / 64) * 8 bytes, i=0..19) = ~128 KB → fits in L2.

| key_bits | Max N | Bitmap size |
|----------|-------|-------------|
| 14 | 16K | 4 KB |
| 17 | 128K | 16 KB |
| 19 | 512K | 128 KB |
| 20 | 1M | 256 KB |
| 24 | 16M | 4 MB |

#### Operations

```rust
impl PrefixBitmap {
    /// Test if prefix exists at level L.
    fn test(&self, level: usize, prefix_bits: u64) -> bool {
        let word = prefix_bits / 64;
        let bit = prefix_bits % 64;
        (self.levels[level][word as usize].load(Relaxed) >> bit) & 1 == 1
    }

    /// Set prefix bit (called during insert_node).
    fn set(&self, level: usize, prefix_bits: u64) {
        let word = prefix_bits / 64;
        let bit = prefix_bits % 64;
        self.levels[level][word as usize].fetch_or(1 << bit, Release);
    }

    // Never cleared — see rationale below.
}
```

#### Never-clear policy

When `remove_node` deletes prefixes, the bitmap bits are NOT cleared. Rationale:
- Other top-level nodes may share the same prefix (clearing would be incorrect).
- Stale bits only cause an extra hash lookup (bitmap says "exists" but table slot
  has null pointers) — not a correctness issue.
- For growing workloads (the common case), there are zero stale bits.
- For mixed insert/delete workloads, stale bits accumulate slowly and only
  degrade to the no-bitmap baseline in the worst case.

#### Modified find_lowest_ancestor

```rust
fn find_lowest_ancestor(&self, key: &T) -> Option<*mut SkipTrieNode<T>> {
    let bits = self.key_to_bits(key);
    let mut best_node: *mut SkipTrieNode<T> = ptr::null_mut();
    let mut start = 0;
    let mut size = self.key_bits / 2;

    while size > 0 {
        let level = start + size;
        let prefix_bits = bits >> (self.key_bits - level);

        // Bitmap check: L2 cache hit, ~5ns
        if self.bitmap.test(level, prefix_bits) {
            // Hash lookup: only when bitmap confirms existence
            let prefix_key = Self::encode_prefix(bits, level, self.key_bits);
            if let Some(slot) = self.prefixes.find(prefix_key) {
                let direction = Self::bit_at(bits, level, self.key_bits);
                let candidate = slot.pointers[direction].load(Acquire);
                if !candidate.is_null() {
                    best_node = candidate;
                    start += size;
                }
            }
        }

        size /= 2;
    }

    if best_node.is_null() { None } else { Some(best_node) }
}
```

#### Expected impact (on top of Phase 1)

Typical binary search: 5 iterations, ~2 have existing prefixes, ~3 don't.
- Bitmap eliminates 3 hash lookups: saves 3 × 120ns = 360ns
- Remaining: 2 hash lookups + 5 bitmap checks = 2 × 120ns + 5 × 5ns = 265ns

```
find_lowest_ancestor:  265ns   (was 600ns after Phase 1, 2000ns before)
Total 500K insert:     ~1.0s   (was ~2.2s after Phase 1, ~7.2s before)
Ratio vs skiplist:     ~30x    (was 67x after Phase 1, 219x before)
```

### Phase 3: Validation

1. All existing skip_trie tests pass (`cargo test -p starfish-core`)
2. Crossbeam epoch-guarded tests pass (`cargo test -p starfish-crossbeam`)
3. Benchmark regression suite:
   - Batch insert scaling: 1K / 10K / 100K / 200K
   - Multithreaded insert: 1/4/16 threads × 1000 keys
   - Contention: 100 keys, insert+delete, 1/16 threads
4. Compare ratios against plain SkipList at each scale

### Summary

| | Current | Phase 1 | Phase 1+2 |
|---|---------|---------|-----------|
| find_lowest_ancestor (500K) | 2000 ns | 600 ns | 265 ns |
| Total 500K batch insert | ~7.2s | ~2.2s | ~1.0s |
| Ratio vs SkipList | 219x | ~67x | ~30x |
| Memory (hash table, 500K) | ~10 MB | ~6 MB | ~6 MB + 128 KB |

The SkipTrie will still be slower than a plain skiplist (the O(log log u) depth advantage
doesn't overcome the per-operation overhead), but 30x is far more practical than 219x,
and at smaller scales (N ≤ 10K) where the table fits in cache, the ratio should drop
to 2-3x.
