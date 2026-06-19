# SkipTree: Cache-Conscious Skip List with Fat Leaf Nodes

**Status:** PROPOSAL
**Crate:** starfish-core (new data structure)
**Key files:** (not yet created)

## Motivation

The current `SkipList` is **memory-latency-bound**. Every level-0 traversal step
chases a pointer to a separately-allocated node, typically incurring an L2/L3 cache
miss per key comparison. For workloads that scan ranges or search dense keyspaces,
this pointer-chasing overhead dominates.

A hybrid design — skip-list express lanes on top, B+tree-style sorted leaf blocks
at the bottom — keeps O(log n) navigation but replaces the most expensive part
(level-0 scanning) with cache-friendly sequential access and SIMD-accelerated search.

## Design Overview

```
Index levels (sparse, pointer-based — same as SkipList today):

  Level 2:   [5] ──────────────────────────────────── [200] ────
  Level 1:   [5] ──────────── [50] ────────────────── [200] ────

Leaf level (dense, sorted arrays linked in a chain):

  Level 0:   [1,3,5,7,9,12,15] → [18,20,22,30,45,47,50] → [52,55,...,200] → ...
              └── LeafBlock ──┘    └──── LeafBlock ──────┘   └─ LeafBlock ─┘
```

### Two node types

| Type | Layout | Purpose |
|------|--------|---------|
| **IndexNode** | Same as current `SkipNode` (FAM pointers, one key) | Express-lane navigation at levels 1+ |
| **LeafBlock** | Sorted array of B keys + next pointer + count | Dense storage at level 0 |

### LeafBlock layout

```rust
#[repr(C)]
struct LeafBlock<T> {
    count: u16,                        // Number of live keys (0..B)
    flags: AtomicU16,                  // Split-in-progress, frozen, etc.
    next: AtomicPtr<LeafBlock<T>>,     // Forward chain (level 0)
    keys: [MaybeUninit<T>; B],         // Sorted array, first `count` slots populated
}
```

**Block size B:** 16 is the sweet spot for `u64` keys.
- 16 × 8 = 128 bytes of keys = two cache lines
- One AVX2 `_mm256_cmpgt_epi64` covers 4 keys → 4 SIMD ops scan the full block
- For variable-size keys (String), B = 8 may be better to keep blocks compact

### IndexNode layout

Identical to today's `SkipNode`, but level-0 pointers point to `LeafBlock` instead
of another `SkipNode`. The index key is the *first key* (minimum) of the referenced
leaf block.

## Operations

### Search(key)

```
1. Traverse index levels top-down (same as SkipList.find_node_internal)
   → arrive at the LeafBlock whose min_key <= key
2. Within the LeafBlock:
   a. SIMD path (u64 keys, x86_64):
      broadcast key → cmpgt against 4-wide lanes → movemask → trailing_zeros
   b. Scalar fallback:
      binary search or linear scan over keys[0..count]
3. If not found, follow leaf.next and check the next block
   (key could be between this block's max and next block's min)
```

**Complexity:** O(log n) index traversal + O(B) leaf scan (effectively O(1) with SIMD)

### Insert(key)

```
1. Search → find target LeafBlock
2. Find insertion position within the block (binary/SIMD search)
3. If count < B:
   a. Shift keys[pos..count] right by one
   b. Write key at position
   c. Increment count
4. If count == B (block full):
   a. Split: allocate new LeafBlock, move upper half of keys
   b. Link new block into the leaf chain (CAS next pointer)
   c. Insert a new IndexNode for the new block's min_key
      (random height, same as SkipList insert)
   d. Insert key into the appropriate half
```

**Complexity:** O(log n) + O(B) shift. With B=16, the shift is 1-2 cache lines.

### Delete(key)

```
1. Search → find target LeafBlock and position
2. Shift keys[pos+1..count] left by one
3. Decrement count
4. If count == 0:
   a. Unlink the empty block from the leaf chain
   b. Remove the corresponding IndexNode (same as SkipList delete)
5. Optional: merge with neighbor if count < B/4 (deferred, not critical)
```

### Range scan

This is where the design really shines:

```
1. Search for range start → land in a LeafBlock at some position
2. Scan forward within the block (contiguous memory, prefetch-friendly)
3. Follow leaf.next pointers (sequential access pattern)
4. Stop when key > range end
```

The leaf chain turns range scans from N pointer dereferences into N/B block
reads with sequential prefetching. For B=16, that's a 16× reduction in cache
misses.

## Lock-Free Considerations

This is the hardest part of the design. The current SkipList achieves lock-free
insert/delete because each node is independently CAS-linkable. Fat leaf blocks
introduce **intra-block mutation** (shifts) and **splits**, which are harder to
make lock-free.

### Approach 1: Fine-grained locking per LeafBlock

Simplest. Each LeafBlock has a spinlock. Index levels remain lock-free.
Contention is low because the lock scope is one block (microseconds).

- Pro: Simple, correct, good enough for most workloads
- Con: Not lock-free, potential priority inversion under contention

### Approach 2: Copy-on-Write leaf blocks

On mutation, copy the block, apply the change, CAS the index pointer.

```
1. Read current block → copy to new allocation
2. Apply insert/delete to the copy
3. CAS index_node.pointer[0]: old_block → new_block
4. Defer deallocation of old_block via epoch/guard
```

- Pro: True lock-free, readers never block
- Con: O(B) copy per mutation, memory pressure from short-lived copies
- Sweet spot: small B (8-16) where copy cost is negligible

### Approach 3: Delta chains (BwTree-style)

Append delta records to the block instead of modifying it. Periodically
consolidate deltas into a fresh block.

- Pro: Lock-free, amortized O(1) insert
- Con: Significant complexity, consolidation pauses, pointer indirection

### Recommendation

**Start with Approach 2 (CoW)** for the initial implementation. At B=16 with
`u64` keys, copying 128 bytes is a single `memcpy` — cheaper than the CAS
itself. If profiling shows copy overhead matters, explore delta chains later.

## SIMD Search Within a LeafBlock

### x86_64 (AVX2) — 4 keys per instruction for u64

```rust
use core::arch::x86_64::*;

/// Returns the index of the first key >= target, or count if all keys < target.
#[target_feature(enable = "avx2")]
unsafe fn simd_lower_bound_u64(keys: &[u64; 16], count: usize, target: u64) -> usize {
    let target_vec = _mm256_set1_epi64x(target as i64);

    // Process 4 keys at a time (4 iterations for 16 keys)
    for chunk in 0..4 {
        let offset = chunk * 4;
        if offset >= count { return count; }

        let keys_vec = _mm256_loadu_si256(keys.as_ptr().add(offset) as *const __m256i);
        // Compare: keys[i] >= target  ↔  !(keys[i] < target)
        let lt = _mm256_cmpgt_epi64(target_vec, keys_vec); // target > keys[i]
        let mask = _mm256_movemask_epi8(lt) as u32;

        if mask != 0xFFFF_FFFF {
            // At least one key >= target in this chunk
            // Each 64-bit lane produces 8 mask bits
            let lane_mask = !mask & 0xFF_FF_FF_FF;
            let first_ge = (lane_mask.trailing_zeros() / 8) as usize;
            let pos = offset + first_ge;
            return if pos < count { pos } else { count };
        }
    }
    count
}
```

### aarch64 (NEON) — 2 keys per instruction for u64

```rust
use core::arch::aarch64::*;

#[target_feature(enable = "neon")]
unsafe fn simd_lower_bound_u64_neon(keys: &[u64; 16], count: usize, target: u64) -> usize {
    let target_vec = vdupq_n_u64(target);

    for chunk in 0..8 {
        let offset = chunk * 2;
        if offset >= count { return count; }

        let keys_vec = vld1q_u64(keys.as_ptr().add(offset));
        let ge_mask = vcgeq_u64(keys_vec, target_vec);

        if vmaxvq_u32(vreinterpretq_u32_u64(ge_mask)) != 0 {
            // Found a key >= target
            let lane0 = vgetq_lane_u64(ge_mask, 0);
            if lane0 != 0 { return offset; }
            return offset + 1;
        }
    }
    count
}
```

### Scalar fallback

```rust
fn scalar_lower_bound<T: Ord>(keys: &[T], count: usize, target: &T) -> usize {
    keys[..count].partition_point(|k| k < target)
}
```

For generic `T: Ord`, always use the scalar path. SIMD paths are only available
via specialization or explicit type-specific constructors
(`SkipTree::<u64>::new()`).

## Comparison with Existing Structures

| Property | SkipList | SkipTree (proposed) | B+Tree |
|----------|----------|---------------------|--------|
| Navigation | O(log n) pointer chase | O(log n) pointer chase | O(log n) pointer chase |
| Leaf search | 1 compare + 1 pointer | SIMD scan over B keys | binary search over B keys |
| Insert | O(1) CAS link | O(B) shift or CoW copy | O(B) shift + possible split |
| Range scan | N pointer derefs | N/B block reads | N/B block reads |
| Lock-free | Yes (fully) | Yes (CoW leaves) | Hard (BwTree etc.) |
| Rebalancing | None (probabilistic) | Split only (no merge required) | Split + merge |
| Memory overhead | 1 ptr/key/level | 1 ptr/B keys at level 0 | 1 ptr/B keys |
| SIMD benefit | None | 4-8× within leaves | Possible but less natural |
| Batch insert | O(1) amortized | O(B) per block | O(B) per page |

### When to use which

- **SkipList**: Insert/delete-heavy workloads, small datasets that fit in cache,
  maximum concurrency (fully lock-free)
- **SkipTree**: Read-heavy or range-scan-heavy workloads, large datasets where
  cache misses dominate, integer keys where SIMD applies
- **B+Tree**: Disk-backed storage, page-aligned I/O, when you need strict balancing

## Implementation Plan

### Phase 1: Scalar SkipTree

1. Define `LeafBlock<T>` with sorted array + next pointer
2. Define `IndexNode<T>` (reuse `SkipNode` with level-0 pointing to `LeafBlock`)
3. Implement search (index traversal → scalar binary search in leaf)
4. Implement insert with CoW leaf mutation and split
5. Implement delete with CoW leaf mutation
6. Implement `SortedCollection` trait
7. Add to `common_tests` harness for correctness verification

### Phase 2: SIMD specialization

1. Add SIMD `lower_bound` for `u64`, `i64`, `u32`, `i32` keys
2. Runtime feature detection (`is_x86_feature_detected!("avx2")`)
3. Benchmark: SkipList vs SkipTree at 1K, 100K, 1M, 10M keys
4. Benchmark: range scan throughput comparison

### Phase 3: Lock-free hardening

1. Stress-test CoW path under high contention (8-32 threads)
2. Profile and optimize: are copies the bottleneck? Consider delta chains if so
3. Integrate with `starfish-epoch` / `starfish-crossbeam` guards for deferred free

### Phase 4: MapCollection integration

1. Implement `MapCollection<K, V>` for `SkipTree<Pair<K,V>, G>`
2. Add to `map_collection_tests` harness
3. Benchmark against `SplitOrderedHashMap` for point lookups

## References

- Kim et al., "FAST: Fast Architecture Sensitive Tree Search on Modern CPUs and GPUs", SIGMOD 2010
- Levandoski et al., "The Bw-Tree: A B-tree for New Hardware Platforms", ICDE 2013
- Kohler et al., "Masstree: A Cache-Friendly Mashup of Tries and B-Trees" (unpublished)
- Herlihy et al., "A Provably Correct Scalable Concurrent Skip List", 2006
- Braginsky & Petrank, "A Lock-Free B+Tree", SPAA 2012
