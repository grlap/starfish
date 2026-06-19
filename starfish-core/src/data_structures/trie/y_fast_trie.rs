//! Y-Fast Trie: a concurrent key-value map with sorted-bucket partitioning.
//!
//! Based on Willard, "Log-Logarithmic Worst-Case Range Queries Are Possible
//! in Space Θ(N)", 1982.
//!
//! Keys are partitioned into small sorted buckets (~log(u) contiguous elements
//! each), with a sorted representative table mapping representative keys to
//! buckets.
//!
//! Buckets use copy-on-write `AtomicPtr<Vec<Pair<K,V>>>`: reads are lock-free
//! (atomic load + scan), writes clone the Vec and CAS-swap the pointer.
//! The representative table is also lock-free (`AtomicPtr<Vec<...>>`), with
//! splits coordinated by a `SplitDescriptor` (S-note) and published via
//! CoW + CAS. Any thread can cooperatively help complete an in-progress split.
//!
//! # Design deviations from the classical Y-fast trie
//!
//! 1. **Representative index**: The classical design uses an x-fast trie (hash
//!    table with prefix entries) for O(log log u) successor lookup. This
//!    implementation uses a sorted `Vec<(u64, *mut YFastBucket)>` behind an
//!    `AtomicPtr` with CoW semantics — binary search gives O(log(n/log u))
//!    successor lookup, avoiding the hash table cache-miss problem entirely.
//!
//! 2. **Lock-free buckets (no RwLock phase)**: Buckets use CoW
//!    `AtomicPtr<Vec<Pair<K,V>>>` directly, skipping the simpler RwLock
//!    approach.
//!
//! 3. **No merge**: Bucket merge on underflow is not implemented. Delete-heavy
//!    workloads may accumulate near-empty buckets.

// =============================================================================
// Y-FAST TRIE STRUCTURE & INVARIANTS
// =============================================================================
//
// Reference: Willard, "Log-Logarithmic Worst-Case Range Queries Are Possible
//            in Space Θ(N)", 1982.
//
// The Y-fast trie partitions the key space into contiguous sorted buckets.
// Each bucket has a representative (its max key), stored in a sorted rep table.
// Lookup finds the bucket via binary search on reps, then scans within.
//
// Structure (3 buckets, bucket_max = 4 for illustration):
//
//   Rep table (sorted by rep_bits):
//
//     ┌──────────┬──────────┬──────────┐
//     │ rep=15   │ rep=30   │ rep=MAX  │
//     │ → B0     │ → B1     │ → B2     │
//     └──────────┴──────────┴──────────┘
//
//   Buckets (each is a sorted Vec behind AtomicPtr):
//
//     B0: [3, 7, 12, 15]     ← keys ≤ 15
//     B1: [18, 22, 25, 30]   ← keys ≤ 30
//     B2: [35, 42]           ← keys ≤ MAX
//
// INVARIANTS:
// 1. Every key k is in the bucket whose representative is the smallest rep ≥ k
// 2. Bucket elements are always sorted ascending by key
// 3. No duplicate keys across the entire trie
// 4. A catch-all bucket with rep = MAX always exists (covers unbounded keys)
// 5. bucket.len ≤ bucket_max (4 * ceil_log2(u)), enforced by splitting
// 6. Each bucket's inner Vec is immutable once published — writes create
//    a new Vec and CAS-swap the AtomicPtr (copy-on-write)
//
// =============================================================================
// COPY-ON-WRITE (CoW) READ/WRITE PROTOCOL
// =============================================================================
//
// Reads are lock-free: load the AtomicPtr, binary search the Vec snapshot.
// Writes clone the Vec, modify the clone, then CAS-swap the pointer.
//
// READ (lock-free, never blocks):
// ─────────────────────────────────
//   1. reps_ptr = reps.load(Acquire)        // atomic, never blocks
//   2. binary search reps for successor     // O(log(n/log u))
//   3. ptr = bucket.inner.load(Acquire)     // atomic, never blocks
//   4. binary search (*ptr) for key         // O(log(log u))
//
// WRITE (insert, CAS loop):
// ─────────────────────────────────
//
//   Before:
//     bucket.inner ──► Vec_old: [3, 7, 15]
//                       ↑
//                    readers see this
//
//   Step 1 — Snapshot: old_ptr = bucket.inner.load(Acquire)
//   Step 2 — Clone:    new_vec = (*old_ptr).clone()         // ~60 elements avg
//   Step 3 — Modify:   new_vec.insert(idx, Pair(12, val))
//   Step 4 — Publish:  CAS(bucket.inner, old_ptr, new_ptr)
//
//   After (CAS succeeds):
//     bucket.inner ──► Vec_new: [3, 7, 12, 15]
//                       ↑
//                    new readers see this
//
//                      Vec_old: [3, 7, 15]
//                       ↑
//                    in-flight readers still safe
//                    (freed via guard.defer_destroy)
//
//   After (CAS fails — another thread modified the bucket):
//     Discard new_vec, reload old_ptr, retry from Step 1.
//
// WHY CoW IS SAFE:
// - Readers never see a partially-modified Vec (atomic pointer swap)
// - Old Vec is not freed until all epoch-pinned readers have exited
// - CAS retry is correct: at ~60 elements, clone costs ~100 ns — negligible
//   against the rep table lookup cost
//
// =============================================================================
// BUCKET SPLIT OPERATION
// =============================================================================
//
// Triggered when bucket.len exceeds bucket_max (4 * ceil_log2(u)).
// Coordinated via freeze CAS + SplitDescriptor (S-note); reads never block.
//
// Before split (bucket B has 6 elements, bucket_max = 4):
//
//   Rep table:
//     ┌──────────┬──────────┐
//     │ rep=30   │ rep=MAX  │
//     │ → B      │ → B2     │
//     └──────────┴──────────┘
//
//   B:  [5, 10, 15, 20, 25, 30]
//   B2: [40, 50]
//
// Split at median (index 3):
//
//   Left elements:  [5, 10, 15]        → new bucket B_left, rep = 15
//   Right elements: [20, 25, 30]       → new bucket B_right, rep = 30 (same as old)
//
// After split:
//
//   Rep table:
//     ┌──────────┬──────────┬──────────┐
//     │ rep=15   │ rep=30   │ rep=MAX  │
//     │ → B_left │ → B_right│ → B2     │
//     └──────────┴──────────┴──────────┘
//
//   B_left:  [5, 10, 15]
//   B_right: [20, 25, 30]
//   B2:      [40, 50]
//
// SPLIT DETAILS (lock-free S-note protocol):
//   1. Re-check bucket.len ≤ bucket_max (may be stale), return if within limit
//   2. Freeze CAS: CAS bucket.inner from vec_ptr to null (AcqRel)
//      - Atomically captures the data AND prevents further writer CAS
//      - Writers encountering null load split_note and help if non-null
//      - If another thread already froze (inner is null), load split_note
//        and help complete that split, then return
//   3. bucket.generation.fetch_add(1, Release) — signals in-flight writers
//   4. Partition frozen data at median → allocate left/right YFastBuckets
//   5. Allocate SplitDescriptor (S-note) capturing both buckets, old bucket
//      pointer, old rep bits, median bits, and frozen vec pointer
//   6. Publish S-note: bucket.split_note.store(note_ptr, Release)
//   7. bucket.len.store(0, Relaxed) — prevents re-split by stale len reads
//   8. Call help_complete_split(note) — any thread can call this:
//      a. CAS loop on self.reps: load old reps, find old bucket by pointer,
//         clone reps replacing old entry with right bucket + inserting left
//      b. CAS(self.reps, old_ptr, new_ptr, AcqRel) — publishes new reps
//         - Winner: defer_destroy(old_reps_ptr)
//         - Loser: drop built reps, reload, retry (may find bucket gone → done)
//      c. Exactly-once gate: CAS(note.completed, false, true)
//         - Winner: defer_destroy(old_bucket), defer_destroy(frozen_vec),
//           defer_destroy(note)
//
// WHY SPLIT IS SAFE:
// - Freeze CAS is the tiebreaker: exactly one thread wins per bucket
// - Losers see null inner, load split_note, and cooperatively help complete
// - Generation counter signals writers that loaded gen_before before the freeze
// - Reps pointer is swapped via CAS (AcqRel) — readers see either old or new
//   reps, never a torn pointer. Concurrent splits of different buckets
//   serialize through the CAS loop (loser reloads and retries)
// - Readers encountering null inner load split_note and help, then retry
// - In-flight readers with a pointer to the old Vec are safe: the Vec
//   is not freed until defer_destroy runs (epoch reclamation)
// - Exactly-once cleanup via SplitDescriptor.completed AtomicBool gate
//
// =============================================================================
// CONCURRENCY MODEL
// =============================================================================
//
// Fully lock-free reads; writes use CAS loops + generation counter protocol.
// Splits are lock-free via freeze CAS + SplitDescriptor + cooperative helping.
//
//   Level 1 — Rep table: AtomicPtr<Vec<(rep_bits, bucket_ptr)>>
//     - Reads:  load(Acquire) — lock-free, never blocks
//     - Splits: clone → modify → CAS(Release), cooperative helping via SplitDescriptor
//
//   Level 2 — Bucket data: AtomicPtr<Vec<Pair<K,V>>> (lock-free)
//     - Reads: atomic load, no lock
//     - Writes: CAS loop (clone → modify → swap)
//
//   Level 3 — Split detection: freeze CAS + generation counter
//     - Split:  CAS inner to null (freeze), then fetch_add(gen, Release)
//     - Insert/Remove: bail on null inner; check gen after CAS to detect races
//
// Thread interactions:
//
//   T1: insert(key=12)          T2: insert(key=25)
//   │                           │
//   ├─ reps.load(Acquire)       ├─ reps.load(Acquire)  ← lock-free
//   ├─ find_successor → B0     ├─ find_successor → B1 ← different buckets
//   ├─ CAS loop on B0.inner    ├─ CAS loop on B1.inner ← zero contention
//   └─ done                    └─ done
//
//   T1: insert(key=12)          T2: insert(key=14)     ← same bucket!
//   │                           │
//   ├─ reps.load(Acquire)       ├─ reps.load(Acquire)
//   ├─ find_successor → B0     ├─ find_successor → B0
//   ├─ gen_before = gen.load() ├─ gen_before = gen.load()
//   ├─ CAS succeeds ✓          ├─ CAS fails ✗ (old changed)
//   │                           ├─ check gen — same → retry CAS
//   │                           ├─ CAS succeeds ✓
//   └─ done                    └─ done
//
//   T1: insert triggers split   T2: concurrent insert on same bucket
//   │                           │
//   ├─ freeze CAS (inner→null)  ├─ gen_before = gen.load()
//   ├─ gen.fetch_add(1)         ├─ CAS loop...
//   ├─ partition, build S-note  │
//   │  help_complete_split:     ├─ CAS fails, check gen → changed!
//   │  CAS reps (AcqRel)       ├─ break inner, retry outer loop
//   ├─ defer_destroy old reps   ├─ reload reps.load(Acquire) → new reps
//   ├─ defer_destroy old bucket ├─ find correct bucket, retry insert
//   └─ done                    └─ done
//
// =============================================================================

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use crate::data_structures::pair::Pair;
use crate::guard::Guard;

pub use super::skip_trie::KeyBits;

// =============================================================================
// Constants
// =============================================================================

/// Default key universe size (2^30), same as SkipTrie.
const MAX_U: usize = 1 << 30;

// =============================================================================
// Utilities
// =============================================================================

/// Calculate ceil(log2(u)), capped at 63.
fn ceil_log2_capped(u: usize) -> usize {
    if u <= 1 {
        1
    } else {
        let raw = (usize::BITS - (u - 1).leading_zeros()) as usize;
        raw.min(63)
    }
}

// =============================================================================
// YFastBucket — sorted contiguous storage for ~log(u) elements
// =============================================================================

/// A bucket in the Y-fast trie. Stores a sorted Vec of key-value pairs
/// behind an `AtomicPtr` for lock-free reads and copy-on-write writes.
///
/// The `rep_bits` field is the representative key in bit form — the maximum
/// key this bucket can hold. It is immutable after construction; splits
/// create new buckets rather than modifying existing ones.
struct YFastBucket<K, V> {
    /// Representative key in bit form (max key of this bucket).
    rep_bits: u64,
    /// Copy-on-write sorted storage. Reads: `load(Acquire)` → scan.
    /// Writes: clone → modify → CAS-swap → defer-destroy old.
    inner: AtomicPtr<Vec<Pair<K, V>>>,
    /// Cached element count for fast threshold checks without dereferencing.
    len: AtomicUsize,
    /// Generation counter. Incremented (Release) by `maybe_split` before
    /// reading bucket data. Insert/remove load this (Acquire) before and
    /// after their CAS loop to detect concurrent splits.
    generation: AtomicU64,
    /// Points to the active SplitDescriptor when this bucket is being split.
    /// Null when no split is in progress. Published (Release) after the
    /// splitting thread has frozen the bucket and built the new buckets.
    split_note: AtomicPtr<SplitDescriptor<K, V>>,
}

impl<K: Ord, V> YFastBucket<K, V> {
    fn new(rep_bits: u64) -> Self {
        YFastBucket {
            rep_bits,
            inner: AtomicPtr::new(Box::into_raw(Box::new(Vec::new()))),
            len: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
            split_note: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    fn new_with_elements(rep_bits: u64, elements: Vec<Pair<K, V>>) -> Self {
        let len = elements.len();
        YFastBucket {
            rep_bits,
            inner: AtomicPtr::new(Box::into_raw(Box::new(elements))),
            len: AtomicUsize::new(len),
            generation: AtomicU64::new(0),
            split_note: AtomicPtr::new(std::ptr::null_mut()),
        }
    }
}

impl<K, V> Drop for YFastBucket<K, V> {
    fn drop(&mut self) {
        let ptr = self.inner.load(Ordering::Relaxed);
        if !ptr.is_null() {
            unsafe {
                drop(Box::from_raw(ptr));
            }
        }
    }
}

// =============================================================================
// SplitDescriptor — lock-free split operation descriptor (S-note)
// =============================================================================

/// Captures all state needed to complete a bucket split, enabling any thread
/// to cooperatively help finish the operation.
///
/// Lifecycle:
///   1. Splitting thread freezes bucket (CAS inner → null), bumps generation
///   2. Splitting thread partitions data, creates left/right buckets
///   3. Splitting thread allocates SplitDescriptor, publishes on bucket.split_note
///   4. Any thread calls `help_complete_split` to CAS new reps into `self.reps`
///   5. The thread whose `completed` CAS succeeds performs all defer_destroy cleanup
struct SplitDescriptor<K, V> {
    /// New left bucket (keys ≤ median).
    left_bucket: *mut YFastBucket<K, V>,
    /// New right bucket (keys > median, same rep_bits as old bucket).
    right_bucket: *mut YFastBucket<K, V>,
    /// The old bucket pointer being split (for identification in the reps Vec).
    old_bucket_ptr: *mut YFastBucket<K, V>,
    /// rep_bits of the old bucket (== right bucket's rep_bits).
    old_rep_bits: u64,
    /// rep_bits for the new left bucket (median key).
    median_bits: u64,
    /// The frozen Vec pointer captured during the freeze CAS.
    frozen_vec_ptr: *mut Vec<Pair<K, V>>,
    /// Exactly-once gate: the thread that flips false→true owns cleanup
    /// of the old bucket, frozen Vec, and this descriptor.
    completed: AtomicBool,
}

unsafe impl<K: Send, V: Send> Send for SplitDescriptor<K, V> {}
unsafe impl<K: Send + Sync, V: Send + Sync> Sync for SplitDescriptor<K, V> {}

// =============================================================================
// YFastTrie — main public struct
// =============================================================================

/// A concurrent key-value map based on the Y-fast trie data structure.
///
/// Keys are partitioned into small sorted buckets (~log(u) elements each).
/// Reads are lock-free (atomic load of bucket Vec pointer + scan).
/// Writes use copy-on-write (clone Vec → modify → CAS-swap).
///
/// # Example
///
/// ```ignore
/// use starfish_core::DeferredGuard;
/// use starfish_core::data_structures::trie::y_fast_trie::YFastTrie;
///
/// let trie: YFastTrie<i32, String, DeferredGuard> = YFastTrie::new();
/// trie.insert(42, "hello".to_string());
/// assert_eq!(trie.get(&42), Some("hello".to_string()));
/// ```
pub struct YFastTrie<K, V, G: Guard> {
    /// Lock-free rep table: `load(Acquire)` for reads, CAS for split writes.
    /// Points to a heap-allocated sorted Vec.
    reps: AtomicPtr<Vec<(u64, *mut YFastBucket<K, V>)>>,
    guard: G,
    count: AtomicUsize,
    key_bits: usize,
    /// Maximum bucket size before triggering a split: 4 * ceil_log2(universe_size).
    bucket_max: usize,
}

impl<K, V, G> YFastTrie<K, V, G>
where
    K: KeyBits + Ord + Eq + Clone + Default,
    V: Clone,
    G: Guard,
{
    /// Create a new YFastTrie with the default key universe size (2^30).
    pub fn new() -> Self {
        Self::with_universe_size(MAX_U)
    }

    /// Create a new YFastTrie with the specified key universe size.
    pub fn with_universe_size(key_universe_size: usize) -> Self {
        let key_bits = ceil_log2_capped(key_universe_size);
        let bucket_max = 4 * key_bits;
        Self::with_params(key_universe_size, bucket_max)
    }

    /// Create a new YFastTrie with a custom maximum bucket size.
    ///
    /// Smaller `bucket_max` means faster CoW clones (less data copied per insert)
    /// but more frequent splits. Larger `bucket_max` means fewer splits but
    /// growing clone costs at scale.
    pub fn with_bucket_max(bucket_max: usize) -> Self {
        Self::with_params(MAX_U, bucket_max)
    }

    fn with_params(key_universe_size: usize, bucket_max: usize) -> Self {
        let key_bits = ceil_log2_capped(key_universe_size);

        // Create catch-all bucket covering the entire key space.
        // Its representative is the maximum possible key value in key_bits width.
        let max_rep_bits = if key_bits >= 64 {
            u64::MAX
        } else {
            (1u64 << key_bits) - 1
        };
        let initial_bucket = Box::into_raw(Box::new(YFastBucket::new(max_rep_bits)));
        let initial_reps = Box::into_raw(Box::new(vec![(max_rep_bits, initial_bucket)]));

        YFastTrie {
            reps: AtomicPtr::new(initial_reps),
            guard: G::default(),
            count: AtomicUsize::new(0),
            key_bits,
            bucket_max,
        }
    }

    /// Extract the bottom `key_bits` from the order-preserving bit representation.
    #[inline]
    fn key_to_bits(&self, key: &K) -> u64 {
        let bits = key.to_bits();
        if self.key_bits >= 64 {
            bits
        } else {
            bits & ((1u64 << self.key_bits) - 1)
        }
    }

    /// Find the bucket whose representative is >= key_bits (successor query).
    ///
    /// Binary searches the sorted representative table. Returns the bucket
    /// pointer for the smallest representative >= key_bits.
    #[inline]
    fn find_successor(
        reps: &[(u64, *mut YFastBucket<K, V>)],
        key_bits: u64,
    ) -> Option<*mut YFastBucket<K, V>> {
        let idx = match reps.binary_search_by_key(&key_bits, |&(rep, _)| rep) {
            Ok(i) => i,
            Err(i) => i,
        };
        if idx < reps.len() {
            Some(reps[idx].1)
        } else {
            None
        }
    }

    /// Insert a key-value pair. Returns `true` if inserted, `false` if key already exists.
    pub fn insert(&self, key: K, value: V) -> bool {
        let _epoch = G::pin();
        let key_bits = self.key_to_bits(&key);
        // Set to true after a successful CAS that was invalidated by a
        // split race. On retry, finding the key as a "duplicate" means
        // the split preserved our data — return true (not false).
        let mut prior_cas_raced = false;

        loop {
            let reps_ptr = self.reps.load(Ordering::Acquire);
            let reps = unsafe { &*reps_ptr };

            let bucket_ptr = match Self::find_successor(reps, key_bits) {
                Some(p) => p,
                None => return false, // should never happen (catch-all bucket)
            };

            let bucket = unsafe { &*bucket_ptr };
            let gen_before = bucket.generation.load(Ordering::Acquire);

            // CAS loop: clone Vec → insert → swap pointer
            let mut cas_succeeded = false;
            loop {
                let old_ptr = bucket.inner.load(Ordering::Acquire);
                if old_ptr.is_null() {
                    // Bucket frozen by split — help complete if note published.
                    let note_ptr = bucket.split_note.load(Ordering::Acquire);
                    if !note_ptr.is_null() {
                        self.help_complete_split(unsafe { &*note_ptr });
                    }
                    break;
                }
                let old_vec = unsafe { &*old_ptr };

                match old_vec.binary_search_by(|p| p.key.cmp(&key)) {
                    Ok(_) => {
                        if prior_cas_raced {
                            // A prior CAS was invalidated by a split, but
                            // the split preserved our data. Success.
                            self.count.fetch_add(1, Ordering::Relaxed);
                            return true;
                        }
                        return false; // genuine duplicate
                    }
                    Err(idx) => {
                        let mut new_vec = old_vec.clone();
                        new_vec.insert(idx, Pair::new(key.clone(), value.clone()));
                        let new_ptr = Box::into_raw(Box::new(new_vec));

                        match bucket.inner.compare_exchange(
                            old_ptr,
                            new_ptr,
                            Ordering::Release,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => {
                                bucket.len.fetch_add(1, Ordering::Relaxed);
                                unsafe {
                                    self.guard.defer_destroy(old_ptr, dealloc_vec::<K, V>);
                                }
                                cas_succeeded = true;
                                break;
                            }
                            Err(_) => {
                                unsafe {
                                    drop(Box::from_raw(new_ptr));
                                }
                                // If a split happened, bail out of inner loop to retry outer.
                                if bucket.generation.load(Ordering::Acquire) != gen_before {
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            if !cas_succeeded {
                // CAS failed and generation changed — split in progress. Retry.
                continue;
            }

            // Check if a split raced with our CAS. The generation counter
            // is incremented BEFORE the split reads bucket data (AcqRel),
            // so any split that started before our CAS will have bumped gen.
            if bucket.generation.load(Ordering::Acquire) != gen_before {
                // Our CAS went into a bucket that is being/was split.
                // Retry: if the split preserved our key, we'll find it as
                // a duplicate and return true. If not, we'll re-insert.
                prior_cas_raced = true;
                continue;
            }

            // Normal success path — no split raced with us.
            self.count.fetch_add(1, Ordering::Relaxed);

            let needs_split = bucket.len.load(Ordering::Relaxed) > self.bucket_max;
            if needs_split {
                self.maybe_split(bucket_ptr);
            }

            return true;
        }
    }

    /// Remove a key and return its value. Returns `None` if key not found.
    pub fn remove(&self, key: &K) -> Option<V> {
        let _epoch = G::pin();
        let key_bits = self.key_to_bits(key);

        loop {
            let reps_ptr = self.reps.load(Ordering::Acquire);
            let reps = unsafe { &*reps_ptr };

            let bucket_ptr = Self::find_successor(reps, key_bits)?;

            let bucket = unsafe { &*bucket_ptr };
            let gen_before = bucket.generation.load(Ordering::Acquire);

            // CAS loop: clone Vec → remove → swap pointer
            let result = loop {
                let old_ptr = bucket.inner.load(Ordering::Acquire);
                if old_ptr.is_null() {
                    // Bucket frozen by split — help complete if note published.
                    let note_ptr = bucket.split_note.load(Ordering::Acquire);
                    if !note_ptr.is_null() {
                        self.help_complete_split(unsafe { &*note_ptr });
                    }
                    break None;
                }
                let old_vec = unsafe { &*old_ptr };

                match old_vec.binary_search_by(|p| p.key.cmp(key)) {
                    Ok(idx) => {
                        let value = old_vec[idx].value.clone();
                        let mut new_vec = old_vec.clone();
                        new_vec.remove(idx);
                        let new_ptr = Box::into_raw(Box::new(new_vec));

                        match bucket.inner.compare_exchange(
                            old_ptr,
                            new_ptr,
                            Ordering::Release,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => {
                                bucket.len.fetch_sub(1, Ordering::Relaxed);
                                self.count.fetch_sub(1, Ordering::Relaxed);
                                unsafe {
                                    self.guard.defer_destroy(old_ptr, dealloc_vec::<K, V>);
                                }
                                break Some(value);
                            }
                            Err(_) => {
                                unsafe {
                                    drop(Box::from_raw(new_ptr));
                                }
                                // If a split happened, bail out to retry.
                                if bucket.generation.load(Ordering::Acquire) != gen_before {
                                    break None;
                                }
                            }
                        }
                    }
                    Err(_) => break None,
                }
            };

            // Check if a split raced with our operation.
            if bucket.generation.load(Ordering::Acquire) != gen_before {
                if result.is_some() {
                    // Our CAS removed the key from a bucket being/was split.
                    // The split may or may not have seen our removal.
                    // Undo count and retry — the retry will either find the
                    // key (split used pre-CAS data) and re-remove it, or not
                    // find it (split used post-CAS data) and return None.
                    self.count.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }

            return result;
        }
    }

    /// Check if a key exists (lock-free, retries on frozen bucket).
    pub fn contains_key(&self, key: &K) -> bool {
        let _epoch = G::pin();
        let key_bits = self.key_to_bits(key);

        loop {
            let reps_ptr = self.reps.load(Ordering::Acquire);
            let reps = unsafe { &*reps_ptr };

            let bucket_ptr = match Self::find_successor(reps, key_bits) {
                Some(p) => p,
                None => return false,
            };

            let bucket = unsafe { &*bucket_ptr };
            let vec_ptr = bucket.inner.load(Ordering::Acquire);
            if vec_ptr.is_null() {
                // Bucket frozen by split — help complete if note published.
                let note_ptr = bucket.split_note.load(Ordering::Acquire);
                if !note_ptr.is_null() {
                    self.help_complete_split(unsafe { &*note_ptr });
                }
                continue;
            }
            let vec = unsafe { &*vec_ptr };
            return vec.binary_search_by(|p| p.key.cmp(key)).is_ok();
        }
    }

    /// Get a cloned value for a key.
    pub fn get(&self, key: &K) -> Option<V> {
        self.find_and_apply(key, |_, v| v.clone())
    }

    /// Find a key and apply a function to its key-value pair (lock-free,
    /// retries on frozen bucket).
    pub fn find_and_apply<F, R>(&self, key: &K, f: F) -> Option<R>
    where
        F: Fn(&K, &V) -> R,
    {
        let _epoch = G::pin();
        let key_bits = self.key_to_bits(key);

        loop {
            let reps_ptr = self.reps.load(Ordering::Acquire);
            let reps = unsafe { &*reps_ptr };

            let bucket_ptr = Self::find_successor(reps, key_bits)?;

            let bucket = unsafe { &*bucket_ptr };
            let vec_ptr = bucket.inner.load(Ordering::Acquire);
            if vec_ptr.is_null() {
                // Bucket frozen by split — help complete if note published.
                let note_ptr = bucket.split_note.load(Ordering::Acquire);
                if !note_ptr.is_null() {
                    self.help_complete_split(unsafe { &*note_ptr });
                }
                continue;
            }
            let vec = unsafe { &*vec_ptr };
            return match vec.binary_search_by(|p| p.key.cmp(key)) {
                Ok(idx) => {
                    let pair = &vec[idx];
                    Some(f(&pair.key, &pair.value))
                }
                Err(_) => None,
            };
        }
    }

    /// Get the number of elements.
    pub fn len(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Check if the trie is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Split a bucket that has exceeded the maximum size (lock-free).
    ///
    /// Freezes the bucket via CAS (inner → null), bumps its generation
    /// counter to signal in-flight writers, partitions the data into two
    /// new buckets, publishes a `SplitDescriptor` on `bucket.split_note`,
    /// then calls `help_complete_split` to CAS the new reps into place.
    ///
    /// Any thread encountering a frozen bucket (null inner) can read the
    /// descriptor and cooperatively help complete the split.
    fn maybe_split(&self, bucket_ptr: *mut YFastBucket<K, V>) {
        let bucket = unsafe { &*bucket_ptr };

        // Pre-check — another thread may have already split.
        if bucket.len.load(Ordering::Relaxed) <= self.bucket_max {
            return;
        }

        // Freeze the bucket by swapping inner to null. This atomically
        // captures the current data AND prevents any further writer CAS
        // from succeeding. Exactly one thread wins the freeze CAS and
        // becomes the "splitter" for this bucket.
        let old_vec_ptr = loop {
            let current_ptr = bucket.inner.load(Ordering::Acquire);
            if current_ptr.is_null() {
                // Another thread already froze this bucket.
                // Help complete the split if the note is published.
                let note_ptr = bucket.split_note.load(Ordering::Acquire);
                if !note_ptr.is_null() {
                    self.help_complete_split(unsafe { &*note_ptr });
                }
                return;
            }
            let current_vec = unsafe { &*current_ptr };
            if current_vec.len() <= self.bucket_max {
                return;
            }

            match bucket.inner.compare_exchange(
                current_ptr,
                std::ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Successfully froze. Bump generation so writers detect
                    // the split (those that loaded gen_before earlier).
                    bucket.generation.fetch_add(1, Ordering::Release);
                    break current_ptr;
                }
                Err(_) => {
                    // Another writer CAS'd first — retry freeze.
                }
            }
        };

        // We now exclusively own old_vec_ptr's data — no writer can
        // modify it (inner is null, so all writer CAS attempts fail).
        let vec = unsafe { &*old_vec_ptr };

        // Clone-partition at median.
        let median_idx = vec.len() / 2;
        let left_elems: Vec<Pair<K, V>> = vec[..median_idx].to_vec();
        let right_elems: Vec<Pair<K, V>> = vec[median_idx..].to_vec();

        // Compute median representative (max key of left bucket).
        let median_bits = self.key_to_bits(&left_elems.last().unwrap().key);

        // Create new buckets.
        let left_bucket = Box::into_raw(Box::new(YFastBucket::new_with_elements(
            median_bits,
            left_elems,
        )));
        let right_bucket = Box::into_raw(Box::new(YFastBucket::new_with_elements(
            bucket.rep_bits,
            right_elems,
        )));

        // Build and publish the SplitDescriptor. Other threads seeing
        // null inner will load this and help complete the split.
        let note = Box::into_raw(Box::new(SplitDescriptor {
            left_bucket,
            right_bucket,
            old_bucket_ptr: bucket_ptr,
            old_rep_bits: bucket.rep_bits,
            median_bits,
            frozen_vec_ptr: old_vec_ptr,
            completed: AtomicBool::new(false),
        }));

        // Publish: Release ensures all descriptor fields are visible to
        // any thread that loads this pointer with Acquire.
        bucket.split_note.store(note, Ordering::Release);

        // Prevent re-split by a second thread with a stale bucket_ptr.
        bucket.len.store(0, Ordering::Relaxed);

        // Complete the split (publish new reps + cleanup).
        self.help_complete_split(unsafe { &*note });
    }

    /// Cooperatively complete a bucket split described by `note`.
    ///
    /// CAS-loops on `self.reps` to publish the new representative table
    /// containing the split's left and right buckets. The thread whose
    /// reps CAS succeeds defer-destroys the old reps Vec it replaced.
    /// The thread whose `completed` CAS succeeds defer-destroys the old
    /// bucket, frozen Vec, and the descriptor itself.
    fn help_complete_split(&self, note: &SplitDescriptor<K, V>) {
        if note.completed.load(Ordering::Acquire) {
            return;
        }

        loop {
            if note.completed.load(Ordering::Acquire) {
                return;
            }

            let old_reps_ptr = self.reps.load(Ordering::Acquire);
            let old_reps = unsafe { &*old_reps_ptr };

            // Find the old bucket in the current reps table.
            let old_idx = match old_reps
                .iter()
                .position(|&(_, ptr)| ptr == note.old_bucket_ptr)
            {
                Some(idx) => idx,
                None => {
                    // Old bucket no longer in reps — another helper already
                    // published a reps Vec that includes this split. Done.
                    return;
                }
            };

            // Build new reps Vec: replace old entry with right bucket,
            // insert left bucket at the correct sorted position.
            let mut new_reps = old_reps.clone();
            new_reps[old_idx] = (note.old_rep_bits, note.right_bucket);

            let left_pos = new_reps
                .binary_search_by_key(&note.median_bits, |&(rep, _)| rep)
                .unwrap_or_else(|e| e);
            new_reps.insert(left_pos, (note.median_bits, note.left_bucket));

            let new_reps_ptr = Box::into_raw(Box::new(new_reps));

            match self.reps.compare_exchange(
                old_reps_ptr,
                new_reps_ptr,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // We published the split. Defer-destroy the old reps Vec
                    // we replaced (exactly one thread per CAS success).
                    unsafe {
                        self.guard.defer_destroy(old_reps_ptr, dealloc_reps::<K, V>);
                    }

                    // Claim the completed flag for bucket/vec/note cleanup.
                    // Exactly one thread wins this CAS.
                    if note
                        .completed
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        unsafe {
                            self.guard
                                .defer_destroy(note.old_bucket_ptr, dealloc_bucket::<K, V>);
                            self.guard
                                .defer_destroy(note.frozen_vec_ptr, dealloc_vec::<K, V>);
                            self.guard.defer_destroy(
                                note as *const SplitDescriptor<K, V> as *mut SplitDescriptor<K, V>,
                                dealloc_split_note::<K, V>,
                            );
                        }
                    }
                    return;
                }
                Err(_) => {
                    // CAS failed — another split modified reps concurrently.
                    // Drop our built reps and retry.
                    unsafe {
                        drop(Box::from_raw(new_reps_ptr));
                    }
                }
            }
        }
    }
}

impl<K, V, G> Default for YFastTrie<K, V, G>
where
    K: KeyBits + Ord + Eq + Clone + Default,
    V: Clone,
    G: Guard,
{
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Drop
// =============================================================================

impl<K, V, G: Guard> Drop for YFastTrie<K, V, G> {
    fn drop(&mut self) {
        let reps_ptr = self.reps.load(Ordering::Relaxed);
        if !reps_ptr.is_null() {
            let reps = unsafe { &*reps_ptr };
            for &(_, ptr) in reps.iter() {
                if !ptr.is_null() {
                    unsafe {
                        drop(Box::from_raw(ptr));
                    }
                }
            }
            unsafe {
                drop(Box::from_raw(reps_ptr));
            }
        }
    }
}

// =============================================================================
// Send / Sync
// =============================================================================

unsafe impl<K: Send, V: Send, G: Guard> Send for YFastTrie<K, V, G> {}
unsafe impl<K: Send + Sync, V: Send + Sync, G: Guard> Sync for YFastTrie<K, V, G> {}

// =============================================================================
// Deallocation helpers
// =============================================================================

/// Deallocation function for buckets, used with `Guard::defer_destroy`.
///
/// # Safety
/// The pointer must have been allocated with `Box::new`.
unsafe fn dealloc_bucket<K, V>(ptr: *mut YFastBucket<K, V>) {
    unsafe { drop(Box::from_raw(ptr)) };
}

/// Deallocation function for Vec snapshots replaced during CoW writes.
///
/// # Safety
/// The pointer must have been allocated with `Box::new`.
unsafe fn dealloc_vec<K, V>(ptr: *mut Vec<Pair<K, V>>) {
    unsafe { drop(Box::from_raw(ptr)) };
}

/// Deallocation function for reps Vec snapshots replaced during CoW splits.
/// Frees the Vec container only — bucket pointers inside are managed separately.
///
/// # Safety
/// The pointer must have been allocated with `Box::new`.
unsafe fn dealloc_reps<K, V>(ptr: *mut Vec<(u64, *mut YFastBucket<K, V>)>) {
    unsafe { drop(Box::from_raw(ptr)) };
}

/// Deallocation function for SplitDescriptor notes replaced during lock-free splits.
///
/// # Safety
/// The pointer must have been allocated with `Box::new`.
unsafe fn dealloc_split_note<K, V>(ptr: *mut SplitDescriptor<K, V>) {
    unsafe { drop(Box::from_raw(ptr)) };
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::DeferredGuard;

    type TestTrie = YFastTrie<i32, i32, DeferredGuard>;

    #[test]
    fn test_basic_insert_and_get() {
        let trie = TestTrie::new();
        assert!(trie.insert(10, 100));
        assert!(trie.insert(20, 200));
        assert!(trie.insert(30, 300));

        assert_eq!(trie.get(&10), Some(100));
        assert_eq!(trie.get(&20), Some(200));
        assert_eq!(trie.get(&30), Some(300));
        assert_eq!(trie.get(&40), None);
        assert_eq!(trie.len(), 3);
    }

    #[test]
    fn test_duplicate_rejection() {
        let trie = TestTrie::new();
        assert!(trie.insert(10, 100));
        assert!(!trie.insert(10, 200)); // duplicate
        assert_eq!(trie.get(&10), Some(100)); // original value
        assert_eq!(trie.len(), 1);
    }

    #[test]
    fn test_contains_key() {
        let trie = TestTrie::new();
        assert!(!trie.contains_key(&10));
        trie.insert(10, 100);
        assert!(trie.contains_key(&10));
        assert!(!trie.contains_key(&20));
    }

    #[test]
    fn test_remove() {
        let trie = TestTrie::new();
        trie.insert(10, 100);
        trie.insert(20, 200);
        trie.insert(30, 300);

        assert_eq!(trie.remove(&20), Some(200));
        assert_eq!(trie.len(), 2);
        assert!(!trie.contains_key(&20));
        assert!(trie.contains_key(&10));
        assert!(trie.contains_key(&30));

        // Remove nonexistent
        assert_eq!(trie.remove(&20), None);
        assert_eq!(trie.remove(&99), None);
    }

    #[test]
    fn test_find_and_apply() {
        let trie = TestTrie::new();
        trie.insert(10, 100);
        let result = trie.find_and_apply(&10, |k, v| *k + *v);
        assert_eq!(result, Some(110));
        assert_eq!(trie.find_and_apply(&99, |_, _| 0), None);
    }

    #[test]
    fn test_empty() {
        let trie = TestTrie::new();
        assert!(trie.is_empty());
        assert_eq!(trie.len(), 0);
        trie.insert(1, 1);
        assert!(!trie.is_empty());
    }

    #[test]
    fn test_split_correctness() {
        // With default universe size MAX_U = 2^30, key_bits = 30, bucket_max = 120.
        // Insert enough elements to trigger multiple splits.
        let trie = TestTrie::new();
        let n = 200;

        for i in 0..n {
            assert!(trie.insert(i, i * 10));
        }

        assert_eq!(trie.len(), n as usize);

        // Verify all elements are findable after splits.
        for i in 0..n {
            assert_eq!(
                trie.get(&i),
                Some(i * 10),
                "Failed to find key {} after splits",
                i
            );
        }
    }

    #[test]
    fn test_large_scale() {
        let trie = TestTrie::new();
        let n = 10_000;

        for i in 0..n {
            assert!(trie.insert(i, i));
        }
        assert_eq!(trie.len(), n as usize);

        // Verify all present
        for i in 0..n {
            assert!(trie.contains_key(&i), "Missing key {}", i);
        }

        // Remove half
        for i in (0..n).step_by(2) {
            assert_eq!(trie.remove(&i), Some(i));
        }
        assert_eq!(trie.len(), (n / 2) as usize);

        // Verify remaining
        for i in 0..n {
            if i % 2 == 0 {
                assert!(!trie.contains_key(&i), "Should be removed: {}", i);
            } else {
                assert!(trie.contains_key(&i), "Should still exist: {}", i);
            }
        }
    }

    #[test]
    fn test_reverse_order_insert() {
        let trie = TestTrie::new();
        let n = 200;

        for i in (0..n).rev() {
            assert!(trie.insert(i, i));
        }

        for i in 0..n {
            assert_eq!(trie.get(&i), Some(i), "Failed for key {}", i);
        }
    }

    #[test]
    fn test_single_element() {
        let trie = TestTrie::new();
        trie.insert(42, 420);
        assert_eq!(trie.get(&42), Some(420));
        assert_eq!(trie.remove(&42), Some(420));
        assert!(trie.is_empty());
    }

    #[test]
    fn test_concurrent_insert_disjoint() {
        use std::sync::Arc;
        use std::thread;

        let trie = Arc::new(TestTrie::new());
        let num_threads = 8;
        let per_thread = 1000;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let trie = Arc::clone(&trie);
                thread::spawn(move || {
                    let base = t * per_thread;
                    for i in 0..per_thread {
                        let key = base + i;
                        assert!(trie.insert(key, key * 10));
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(trie.len(), (num_threads * per_thread) as usize);

        // Verify all elements
        for i in 0..(num_threads * per_thread) {
            let key = i;
            assert_eq!(trie.get(&key), Some(key * 10), "Missing key {}", key);
        }
    }

    #[test]
    fn test_concurrent_mixed_operations() {
        use std::sync::Arc;
        use std::thread;

        let trie = Arc::new(TestTrie::new());

        // Pre-populate with even keys
        for i in (0..2000).step_by(2) {
            trie.insert(i, i);
        }
        assert_eq!(trie.len(), 1000);

        let num_threads = 8;
        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let trie = Arc::clone(&trie);
                thread::spawn(move || {
                    match t % 3 {
                        0 => {
                            // Insert odd keys
                            for i in (1..2000).step_by(6) {
                                let key = i + (t / 3) * 2;
                                if key < 2000 {
                                    trie.insert(key, key);
                                }
                            }
                        }
                        1 => {
                            // Read existing keys
                            for i in (0..2000).step_by(3) {
                                let _ = trie.get(&i);
                                let _ = trie.contains_key(&i);
                            }
                        }
                        _ => {
                            // Remove some even keys
                            let base = (t / 3) * 200;
                            for i in (base..base + 200).step_by(4) {
                                if i < 2000 {
                                    trie.remove(&i);
                                }
                            }
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Verify consistency: every key that get() returns should also be found by contains_key()
        for i in 0..2000 {
            let key = i;
            let got = trie.get(&key);
            let contains = trie.contains_key(&key);
            if got.is_some() {
                assert!(
                    contains,
                    "get() returned Some but contains_key() false for {}",
                    key
                );
            }
        }
    }

    #[test]
    fn test_concurrent_split_helping() {
        // Forces many threads to insert into overlapping key ranges that
        // trigger bucket splits simultaneously, exercising the S-note
        // cooperative helping protocol.
        use std::sync::{Arc, Barrier};
        use std::thread;

        let trie = Arc::new(TestTrie::new());
        let num_threads = 8;
        let per_thread = 500;
        let barrier = Arc::new(Barrier::new(num_threads));

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let trie = Arc::clone(&trie);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    // All threads insert into the same key space,
                    // maximizing split contention and helping.
                    for i in 0..per_thread {
                        let key = (t * per_thread + i) as i32;
                        trie.insert(key, key * 10);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let expected = num_threads * per_thread;
        assert_eq!(trie.len(), expected);

        // Verify every key is findable after all concurrent splits.
        for t in 0..num_threads {
            for i in 0..per_thread {
                let key = (t * per_thread + i) as i32;
                assert_eq!(
                    trie.get(&key),
                    Some(key * 10),
                    "Key {} lost after concurrent split helping",
                    key
                );
            }
        }
    }
}
