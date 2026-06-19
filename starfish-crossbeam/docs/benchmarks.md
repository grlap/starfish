# Starfish Concurrent Data Structures: Benchmark Comparison

## Overview

This document compares lock-free concurrent data structure implementations:

| | **Starfish SkipList** | **Starfish SkipTrie** | **Starfish Y-fast Trie** | **Crossbeam SkipMap** |
|---|---|---|---|---|
| Crate | `starfish-core` | `starfish-core` | `starfish-core` | `crossbeam-skiplist` v0.1 |
| Type | Set (`SkipList<T>`) | Set (`SkipTrie<T>`) | Map (`YFastTrie<K, V>`) | Map (`SkipMap<K, V>`) |
| Max levels | 32 | log(log(u)) ≈ 4-5 (truncated) | — (flat buckets) | 32 |
| Probability | 0.5 | 0.5 | — | 0.5 |
| Memory reclamation | crossbeam-epoch (shared) | crossbeam-epoch (shared) | Guard trait (epoch or deferred) | crossbeam-epoch |
| Allocator | MiMalloc | MiMalloc | MiMalloc | MiMalloc |
| Atomic update | UPDATE_MARK (single linearization point) | UPDATE_MARK (mark-then-insert via TruncatedSkipList) | CoW Vec CAS per bucket | Not supported (delete + insert) |
| Batch insert | Sorted hint optimization (~10-15%) | Via SortedCollection trait | — | Not supported |
| Deletion protocol | Mark next[0], cooperative unlink | Harris-style mark top-down to next[0] | CoW Vec CAS (remove from sorted vec) | Mark next[0], cooperative unlink |
| Recovery strategy | `recover_pred` (walk up preds array) | `recover_pred` (walk up preds array) | Retry outer loop on gen change | Restart from head |
| `contains()` | `search_key` (no succs array) | `find_predecessor` (top-down) | Hash → bucket → binary search | `search_bound` (lightweight) |

### Key design differences

- **Starfish SkipList** inlines the head sentinel into the struct, avoiding one heap allocation and one pointer indirection on every traversal.
- **Starfish** uses `load_consume` on ARM (relaxed load + compiler fence) vs Acquire loads on x86, optimizing for hardware data-dependency ordering.
- **Both SkipList implementations** use height clamping to prevent new nodes from being much taller than the current list, ensuring gradual height growth and consistent structure quality.
- **Crossbeam** uses optimistic `search_bound` for lookups, tracking only a single `pred` pointer with no arrays. Starfish SkipList uses `search_key` for `contains()`, which tracks predecessors but skips successor tracking.
- **Starfish SkipList and SkipTrie** support atomic in-place `update()` via UPDATE_MARK, avoiding the window where a key is temporarily missing during delete+insert. The protocol was ported from SkipList to SkipTrie's TruncatedSkipList.
- **Starfish SkipTrie** is based on the SkipTrie data structure (Oshman & Shavit, PODC 2013). It combines a truncated skiplist (height = log(log(u)) ≈ 4-5 levels) with an x-fast trie for O(log log u) amortized predecessor queries. The x-fast trie uses order-preserving key bits (via `KeyBits` trait) and `find_lowest_ancestor` is wired into all search paths (insert, delete, find, update, predecessor). Prefix storage uses an open-addressing `PrefixTable` with a per-level `PrefixBitmap` for fast existence checks. SkipTrie beats Crossbeam under contention thanks to Harris-style deletion with cooperative unlinking, and on machines with sufficient cores (Ultra 9) beats Crossbeam in realistic workloads.

## How to run

```bash
# Run all Criterion benchmarks sequentially (one bench target at a time)
cargo bench --package starfish-crossbeam --bench sorted_collection_benchmark && \
cargo bench --package starfish-crossbeam --bench hash_map_benchmark && \
cargo bench --package starfish-crossbeam --bench indexset_concurrent && \
cargo bench --package starfish-crossbeam --bench upsert_operations && \
cargo bench --package starfish-crossbeam --bench y_fast_trie_benchmark
```

See [profile.md](profile.md) for profiling binaries (samply, Instruments, perf, callgrind).

---

### Machine specs

| Machine | CPU | Arch | Cores / Threads | RAM | OS |
|---|---|---|---|---|---|
| Machine 1 | Intel Xeon Gold 6140 @ 2.30GHz | x86-64 | 72C / 144T | 252 GB | Linux 6.17.0-14-generic |
| Machine 2 | Apple M1 | ARM64 (AArch64) | 4P+4E / 8T | 16 GB | macOS 26.3 (Darwin 25.3.0) |
| Machine 3 | Intel Core Ultra 9 285K | x86-64 | 24C / 24T | 64 GB | Windows 11 (10.0.26200) |
| Machine 4 | Intel Xeon E3-1505M v6 @ 3.00GHz | x86-64 | 4C / 8T | 32 GB | Linux 6.17.4-76061704-generic |

All machines: Rust 1.93.1 (2026-02-11), MiMalloc allocator.

**Column convention:** M1 = Machine 1 (Xeon Gold), M2 = Machine 2 (Apple M1), M3 = Machine 3 (Ultra 9). Abbreviations: SL = SkipList, ST = SkipTrie, CB = Crossbeam SkipMap, SLP = SkipList\<Pair\>, HM = SplitOrderedHashMap, YFT = Y-fast Trie, TI = scc::TreeIndex.

---

## Results

### 1. Sequential insert scaling (ns/op)

Single-threaded, keys inserted in ascending order.

| Size | SL · M1 | SL · M2 | SL · M3 | ST · M1 | ST · M2 | ST · M3 | CB · M1 | CB · M2 | CB · M3 |
|---|---|---|---|---|---|---|---|---|---|
| 10K | 111 | 85 | 57 | 158 | 97 | 81 | 142 | 78 | 65 |
| 50K | 124 | 88 | 60 | 156 | 127 | 81 | 149 | 85 | 69 |
| 100K | 125 | 90 | 59 | 158 | 110 | 84 | 150 | 88 | 70 |
| 500K | 129 | 96 | 66 | 170 | 118 | 96 | 158 | 96 | 78 |

### 2. Random insert (500K elements, ns/op)

| SL · M1 | SL · M2 | SL · M3 | ST · M1 | ST · M2 | ST · M3 | CB · M1 | CB · M2 | CB · M3 |
|---|---|---|---|---|---|---|---|---|
| 442 | 339 | 304 | 1294 | 819 | 776 | 497 | 358 | 355 |

### 3. Sequential find (500K elements, ns/op)

| SL · M1 | SL · M2 | SL · M3 | ST · M1 | ST · M2 | ST · M3 | CB · M1 | CB · M2 | CB · M3 |
|---|---|---|---|---|---|---|---|---|
| 87 | 66 | 57 | 603 | 325 | 212 | 106 | 67 | 52 |

### 4. Random find (500K elements, ns/op)

| SL · M1 | SL · M2 | SL · M3 | ST · M1 | ST · M2 | ST · M3 | CB · M1 | CB · M2 | CB · M3 |
|---|---|---|---|---|---|---|---|---|
| 333 | 282 | 291 | 836 | 526 | 466 | 341 | 280 | 295 |

### 5. Find miss (ns/op)

| SL (500K) · M1 | SL (500K) · M2 | SL (500K) · M3 | ST (100K) · M1 † | ST (100K) · M2 † | ST (100K) · M3 † | CB (500K) · M1 | CB (500K) · M2 | CB (500K) · M3 |
|---|---|---|---|---|---|---|---|---|
| 51 | 40 | 29 | 14911 | 12053 | 6666 | 46 | 33 | 22 |

† SkipTrie find-miss capped at 100K — pathologically slow due to full PrefixTable scan on missing keys.

### 6. Mixed insert/delete — high contention (500K ops, key range = 100, ns/op)

| SL · M1 | SL · M2 | SL · M3 | ST · M1 | ST · M2 | ST · M3 | CB · M1 | CB · M2 | CB · M3 |
|---|---|---|---|---|---|---|---|---|
| 73 | 48 | 32 | 66 | 32 | 27 | 178 | 117 | 123 |

---

### 7. Concurrent insert (10K ops/thread, ns/op)

| Threads | SL · M1 | SL · M2 | SL · M3 | ST · M1 | ST · M2 | ST · M3 | CB · M1 | CB · M2 | CB · M3 |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 529 | 103 | 103 | 586 | 120 | 120 | 560 | 96 | 118 |
| 2 | 340 | 120 | 124 | 411 | 175 | 191 | 404 | 151 | 174 |
| 4 | 199 | 169 | 163 | 279 | 272 | 301 | 315 | 266 | 282 |
| 8 | 108 | 300 | 242 | 200 | 714 | 639 | 361 | 728 | 645 |
| 12 | 78 | 435 | 324 | 342 | 1088 | 990 | 323 | 1064 | 967 |
| 16 | 61 | 559 | 411 | 314 | 1443 | 1353 | 169 | 1470 | 1313 |

### 8. Concurrent contention (10K ops/thread, key range = 100, ns/op)

| Threads | SL · M1 | SL · M2 | SL · M3 | ST · M1 | ST · M2 | ST · M3 | CB · M1 | CB · M2 | CB · M3 |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 330 | 51 | 43 | 299 | 38 | 37 | 540 | 118 | 104 |
| 2 | 173 | 54 | 48 | 162 | 44 | 40 | 380 | 210 | 250 |
| 4 | 89 | 63 | 56 | 85 | 51 | 47 | 514 | 343 | 408 |
| 8 | 47 | 104 | 68 | 46 | 85 | 64 | 482 | 941 | 852 |
| 12 | 33 | 165 | 85 | 32 | 134 | 78 | 453 | 1533 | 1251 |
| 16 | 27 | 222 | 98 | 10 | 185 | 91 | 184 | 2137 | 1594 |

---

### 9. Realistic concurrent workload (40 threads, 100K ops/thread)

75% reader threads + 25% writer threads, keys partitioned by thread. All times in ms.

Note: Benchmarks now use dynamic thread counts (available cores). Data below was collected with the previous 40-thread configuration and will be updated on re-run.

**Machine 1 (Xeon Gold 6140, 72C/144T) — 40T:**

| Write% | SL | TI | CB | HM | YFT | ST |
|---|---|---|---|---|---|---|
| 1% | 141.2 | 145.1 | 372.0 | 249.4 | 343.2 | 1995 |
| 10% | 143.1 | 198.6 | 718.8 | 620.6 | 352.0 | 2118 |
| 30% | 240.2 | 376.4 | 557.3 | 671.8 | 860.2 | 2846 |
| 50% | 322.6 | 313.0 | 703.9 | 713.2 | 1443.1 | 3333 |

**Machine 2 (Apple M1, 4P+4E/8T) — 40T:**

| Write% | SL | TI | CB | HM | YFT | ST |
|---|---|---|---|---|---|---|
| 1% | 100.0 | 148.2 | 193.8 | 273.5 | 230.6 | 1951 |
| 10% | 129.5 | 202.4 | 235.2 | 373.1 | 326.1 | 2194 |
| 30% | 165.0 | 282.0 | 320.5 | 592.2 | 644.4 | 2516 |
| 50% | 205.3 | 398.1 | 433.7 | 761.1 | 1025.6 | 2645 |

---

### 10. Update operations (40 threads, 100K ops/thread, 500K elements, ms)

Note: Benchmarks now use dynamic thread counts (available cores). Data below was collected with the previous 40-thread configuration and will be updated on re-run.

**Machine 1 (Xeon Gold 6140, 72C/144T) — 40T:**

| Update% | SL | CB | ST |
|---|---|---|---|
| 100% | 478.6 | 1675.4 | 1324.6 |
| 50% | 440.4 | 873.1 | 954.4 |
| 10% | 302.9 | 521.4 | 667.4 |
| 1% | 216.5 | 270.1 | 435.7 |

**Machine 2 (Apple M1, 4P+4E/8T) — 40T:**

| Update% | SL | CB | ST |
|---|---|---|---|
| 100% | 908.4 | 1615.7 | 1709.4 |
| 50% | 696.5 | 769.2 | 946.3 |
| 10% | 617.7 | 655.8 | 927.2 |
| 1% | 471.0 | 512.0 | 693.8 |

---

## 11. SplitOrderedHashMap vs SkipList\<Pair\>

`cargo bench --package starfish-crossbeam --bench hash_map_benchmark`

### 11a. Sequential operations at 500K (ns/op)

| Operation | SLP · M1 | SLP · M2 | SLP · M3 | HM · M1 | HM · M2 | HM · M3 |
|---|---|---|---|---|---|---|
| Insert | 126 | 92 | 79 | 316 | 282 | 396 |
| Find-hit | 88 | 65 | 53 | 311 | 272 | 409 |
| Find-miss | 53 | 37 | 19 | 327 | 289 | 445 |
| Remove | 102 | 62 | 58 | 234 | 301 | 278 |
| Mixed | 108 | 70 | 70 | 219 | 181 | 305 |

### 11b. Sequential insert scaling (ns/op)

| Size | SLP · M1 | SLP · M2 | SLP · M3 | HM · M1 | HM · M2 | HM · M3 |
|---|---|---|---|---|---|---|
| 10K | 115 | 81 | 73 | 122 | 95 | 68 |
| 50K | 120 | 87 | 76 | 184 | 97 | 91 |
| 100K | 121 | 89 | 78 | 206 | 103 | 114 |
| 500K | 126 | 92 | 79 | 316 | 282 | 396 |

### 11c. Concurrent insert (10K ops/thread, ns/op)

| Threads | SLP · M1 | SLP · M2 | SLP · M3 | HM · M1 | HM · M2 | HM · M3 |
|---|---|---|---|---|---|---|
| 1 | 670 | 133 | 131 | 726 | 139 | 144 |
| 2 | 454 | 176 | 177 | 510 | 214 | 229 |
| 4 | 272 | 270 | 261 | 359 | 383 | 475 |
| 8 | 209 | 763 | 547 | 286 | 1183 | 1235 |
| 16 | 189 | 1546 | 1025 | 639 | 2604 | 3159 |
| 32 | 130 | 3140 | 2013 | 369 | 6366 | 7467 |
| 64 | 254 | 6284 | 3875 | 541 | 14413 | 14575 |
| 144 | 169 | 14287 | 8778 | 518 | 40794 | 36086 |

### 11d. Concurrent mixed (10K ops/thread, 50% insert / 25% find / 25% remove, ns/op)

| Threads | SLP · M1 | SLP · M2 | SLP · M3 | HM · M1 | HM · M2 | HM · M3 |
|---|---|---|---|---|---|---|
| 1 | 501 | 94 | 102 | 531 | 93 | 103 |
| 2 | 289 | 119 | 137 | 336 | 142 | 158 |
| 4 | 237 | 183 | 191 | 461 | 230 | 284 |
| 8 | 111 | 402 | 312 | 162 | 611 | 638 |
| 16 | 89 | 746 | 552 | 157 | 1303 | 1433 |
| 32 | 152 | 1440 | 980 | 155 | 3665 | 3656 |
| 64 | 129 | 2855 | 1832 | 310 | 8692 | 8917 |
| 144 | 84 | 6186 | 3837 | 291 | 25515 | 22790 |

### 11e. Concurrent contention (10K ops/thread, key range = 100, ns/op)

| Threads | SLP · M1 | SLP · M2 | SLP · M3 | HM · M1 | HM · M2 | HM · M3 |
|---|---|---|---|---|---|---|
| 1 | 352 | 51 | 52 | 386 | 70 | 58 |
| 2 | 192 | 58 | 56 | 222 | 77 | 63 |
| 4 | 102 | 74 | 63 | 128 | 90 | 70 |
| 8 | 54 | 107 | 79 | 76 | 137 | 86 |
| 16 | 11 | 196 | 108 | 44 | 236 | 116 |
| 32 | 10 | 390 | 181 | 17 | 438 | 195 |
| 64 | 12 | 711 | 324 | 14 | 884 | 358 |
| 144 | 11 | 1546 | 684 | 12 | 1817 | 749 |

---

## 12. Y-fast trie results

`cargo bench --package starfish-crossbeam --bench y_fast_trie_benchmark`

Compared with: SplitOrderedHashMap (HM), SkipList\<Pair\> (SLP), Crossbeam SkipMap (CB).

### 12a. Sequential insert scaling (ns/op)

| Size | SLP · M1 | SLP · M2 | SLP · M3 | CB · M1 | CB · M2 | CB · M3 | HM · M1 | HM · M2 | HM · M3 | YFT · M1 | YFT · M2 | YFT · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10K | 122 | 82 | 57 | 149 | 78 | 68 | 132 | 95 | 62 | 256 | 167 | 126 |
| 50K | 129 | 87 | 59 | 160 | 85 | 69 | 183 | 97 | 76 | 295 | 178 | 133 |
| 100K | 130 | 89 | 60 | 161 | 86 | 71 | 199 | 110 | 98 | 308 | 191 | 136 |
| 500K | 137 | 94 | 68 | 171 | 94 | 81 | 323 | 290 | 387 | 439 | 264 | 179 |

### 12b. Sequential find-hit scaling (ns/op)

| Size | YFT · M1 | YFT · M2 | YFT · M3 | HM · M1 | HM · M2 | HM · M3 | SLP · M1 | SLP · M2 | SLP · M3 | CB · M1 | CB · M2 | CB · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10K | 47 | 22 | 21 | 70 | 49 | 34 | 74 | 56 | 34 | 83 | 56 | 39 |
| 50K | 53 | 26 | 20 | 107 | 57 | 43 | 78 | 60 | 37 | 88 | 60 | 41 |
| 100K | 56 | 27 | 22 | 123 | 65 | 55 | 81 | 61 | 38 | 89 | 62 | 41 |
| 500K | 65 | 32 | 28 | 292 | 307 | 312 | 86 | 66 | 41 | 95 | 69 | 45 |

### 12c. Sequential find-miss scaling (ns/op)

| Size | YFT · M1 | YFT · M2 | YFT · M3 | SLP · M1 | SLP · M2 | SLP · M3 | CB · M1 | CB · M2 | CB · M3 | HM · M1 | HM · M2 | HM · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10K | 50 | 23 | 19 | 37 | 27 | 14 | 42 | 26 | 17 | 76 | 49 | 39 |
| 50K | 55 | 26 | 21 | 42 | 34 | 16 | 47 | 32 | 19 | 123 | 57 | 50 |
| 100K | 57 | 28 | 22 | 42 | 33 | 17 | 42 | 29 | 17 | 144 | 66 | 63 |
| 500K | 67 | 33 | 28 | 51 | 37 | 19 | 46 | 33 | 21 | 334 | 315 | 356 |

### 12d. Sequential remove scaling (ns/op)

| Size | SLP · M1 | SLP · M2 | SLP · M3 | CB · M1 | CB · M2 | CB · M3 | HM · M1 | HM · M2 | HM · M3 | YFT · M1 | YFT · M2 | YFT · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10K | 96 | 50 | 56 | 166 | 60 | 77 | 103 | 66 | 51 | 122 | 81 | 54 |
| 50K | 97 | 51 | 56 | 169 | 61 | 76 | 130 | 69 | 57 | 126 | 86 | 56 |
| 100K | 98 | 52 | 56 | 176 | 63 | 79 | 138 | 75 | 72 | 129 | 89 | 59 |
| 500K | 99 | 54 | 56 | 185 | 67 | 82 | 227 | 227 | 306 | 139 | 100 | 66 |

### 12e. Sequential mixed (25% each: insert, find, remove, find-miss, ns/op)

| Size | SLP · M1 | SLP · M2 | SLP · M3 | CB · M1 | CB · M2 | CB · M3 | HM · M1 | HM · M2 | HM · M3 | YFT · M1 | YFT · M2 | YFT · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10K | 100 | 63 | 46 | 133 | 66 | 58 | 99 | 69 | 49 | 154 | 95 | 72 |
| 50K | 105 | 66 | 48 | 148 | 70 | 62 | 141 | 75 | 72 | 165 | 98 | 78 |
| 100K | 108 | 67 | 48 | 150 | 69 | 63 | 152 | 78 | 66 | 172 | 105 | 80 |
| 500K | 110 | 70 | 52 | 154 | 73 | 68 | 213 | 198 | 208 | 225 | 136 | 102 |

### 12f. Concurrent insert (10K ops/thread, ns/op)

| Threads | SLP · M1 | SLP · M2 | SLP · M3 | CB · M1 | CB · M2 | CB · M3 | HM · M1 | HM · M2 | HM · M3 | YFT · M1 | YFT · M2 | YFT · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 517 | 95 | 84 | 573 | 92 | 94 | 585 | 102 | 108 | 645 | 171 | 154 |
| 2 | 384 | 130 | 153 | 470 | 145 | 191 | 467 | 162 | 274 | 479 | 223 | 298 |
| 4 | 398 | 234 | 306 | 444 | 256 | 352 | 323 | 278 | 541 | 504 | 310 | 436 |
| 8 | 339 | 557 | 594 | 204 | 669 | 684 | 251 | 842 | 1048 | 342 | 981 | 649 |

### 12g. Concurrent mixed (10K ops/thread, 33% insert / 33% find / 33% remove, ns/op)

| Threads | SLP · M1 | SLP · M2 | SLP · M3 | HM · M1 | HM · M2 | HM · M3 | CB · M1 | CB · M2 | CB · M3 | YFT · M1 | YFT · M2 | YFT · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 424 | 74 | 71 | 435 | 73 | 91 | 588 | 81 | 89 | 513 | 96 | 97 |
| 2 | 252 | 98 | 144 | 306 | 106 | 191 | 298 | 120 | 176 | 300 | 128 | 206 |
| 4 | 146 | 166 | 222 | 194 | 172 | 307 | 259 | 184 | 252 | 179 | 171 | 267 |
| 8 | 174 | 336 | 342 | 367 | 452 | 566 | 113 | 379 | 369 | 113 | 425 | 369 |

### 12h. Concurrent contention (10K ops/thread, key range = 100, ns/op)

| Threads | YFT · M1 | YFT · M2 | YFT · M3 | SLP · M1 | SLP · M2 | SLP · M3 | HM · M1 | HM · M2 | HM · M3 | CB · M1 | CB · M2 | CB · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 172 | 14 | 19 | 274 | 33 | 32 | 325 | 53 | 39 | 561 | 117 | 104 |
| 2 | 88 | 16 | 23 | 143 | 37 | 36 | 177 | 58 | 44 | 378 | 204 | 262 |
| 4 | 46 | 21 | 32 | 77 | 41 | 45 | 107 | 66 | 53 | 260 | 347 | 492 |
| 8 | 22 | 38 | 40 | 37 | 72 | 56 | 47 | 106 | 64 | 298 | 883 | 910 |

### 12i. Bucket size sensitivity — insert (ns/op)

| Size | bmax=60 · M1 | bmax=60 · M2 | bmax=60 · M3 | bmax=120 · M1 | bmax=120 · M2 | bmax=120 · M3 | bmax=240 · M1 | bmax=240 · M2 | bmax=240 · M3 | bmax=480 · M1 | bmax=480 · M2 | bmax=480 · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10K | 204 | 129 | 100 | 282 | 169 | 125 | 426 | 230 | 168 | 670 | 352 | 270 |
| 100K | 295 | 214 | 137 | 316 | 190 | 138 | 433 | 240 | 175 | 698 | 360 | 272 |
| 500K | 757 | 444 | 299 | 428 | 256 | 177 | 465 | 265 | 186 | 744 | 373 | 277 |

### 12j. Bucket size sensitivity — find-hit at 500K (ns/op)

| bmax=60 · M1 | bmax=60 · M2 | bmax=60 · M3 | bmax=120 · M1 | bmax=120 · M2 | bmax=120 · M3 | bmax=240 · M1 | bmax=240 · M2 | bmax=240 · M3 | bmax=480 · M1 | bmax=480 · M2 | bmax=480 · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 67 | 33 | 29 | 66 | 32 | 28 | 67 | 32 | 28 | 64 | 33 | 26 |

### 12k. Bucket size sensitivity — mixed at 500K (ns/op)

| bmax=8 · M1 | bmax=8 · M2 | bmax=8 · M3 | bmax=16 · M1 | bmax=16 · M2 | bmax=16 · M3 | bmax=32 · M1 | bmax=32 · M2 | bmax=32 · M3 | bmax=60 · M1 | bmax=60 · M2 | bmax=60 · M3 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 30202 | 8000 | 11251 | 4228 | 1920 | 1550 | 938 | 515 | 358 | 348 | 212 | 143 |
