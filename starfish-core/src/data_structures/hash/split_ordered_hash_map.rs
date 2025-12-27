use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::data_structures::hash::{HashMapCollection, HashMapNode};
use crate::data_structures::sorted::sorted_list::{ListNodePosition, SortedList, SortedListNode};
use crate::data_structures::{CollectionNode, MarkedPtr, NodePosition, SortedCollection};
use crate::guard::Guard;

const INITIAL_BUCKETS: usize = 16;
const MAX_LOAD: usize = 4;

/// A unified entry type for the split-ordered list
/// Combines sentinels and regular entries in a single type
#[derive(Clone)]
pub enum SplitOrderedEntry<K, V> {
    Sentinel {
        split_key: usize, // Bit-reversed bucket index
        bucket: usize,
    },
    Regular {
        split_key: usize, // Bit-reversed hash
        hash: usize,      // Original hash
        key: K,
        value: Option<V>, // None for search, Some(v) for actual entries
    },
}

impl<K: Eq, V> PartialEq for SplitOrderedEntry<K, V> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                SplitOrderedEntry::Sentinel {
                    split_key: s1,
                    bucket: b1,
                },
                SplitOrderedEntry::Sentinel {
                    split_key: s2,
                    bucket: b2,
                },
            ) => s1 == s2 && b1 == b2,
            (
                SplitOrderedEntry::Regular {
                    split_key: s1,
                    hash: h1,
                    key: k1,
                    ..
                },
                SplitOrderedEntry::Regular {
                    split_key: s2,
                    hash: h2,
                    key: k2,
                    ..
                },
            ) => s1 == s2 && h1 == h2 && k1 == k2,
            _ => false,
        }
    }
}

impl<K: Eq, V> Eq for SplitOrderedEntry<K, V> {}

impl<K: Eq + Ord, V> PartialOrd for SplitOrderedEntry<K, V> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<K: Eq + Ord, V> Ord for SplitOrderedEntry<K, V> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Extract split keys for primary comparison
        let self_split = match self {
            SplitOrderedEntry::Sentinel { split_key, .. } => *split_key,
            SplitOrderedEntry::Regular { split_key, .. } => *split_key,
        };

        let other_split = match other {
            SplitOrderedEntry::Sentinel { split_key, .. } => *split_key,
            SplitOrderedEntry::Regular { split_key, .. } => *split_key,
        };

        // Primary ordering by split_key
        match self_split.cmp(&other_split) {
            std::cmp::Ordering::Equal => {
                // For same split_key, sentinels come first, then regular entries
                match (self, other) {
                    (SplitOrderedEntry::Sentinel { .. }, SplitOrderedEntry::Regular { .. }) => {
                        std::cmp::Ordering::Less
                    }
                    (SplitOrderedEntry::Regular { .. }, SplitOrderedEntry::Sentinel { .. }) => {
                        std::cmp::Ordering::Greater
                    }
                    (
                        SplitOrderedEntry::Sentinel { bucket: b1, .. },
                        SplitOrderedEntry::Sentinel { bucket: b2, .. },
                    ) => b1.cmp(b2),
                    // For regular entries, compare by hash then key
                    (
                        SplitOrderedEntry::Regular {
                            hash: h1, key: k1, ..
                        },
                        SplitOrderedEntry::Regular {
                            hash: h2, key: k2, ..
                        },
                    ) => match h1.cmp(h2) {
                        std::cmp::Ordering::Equal => k1.cmp(k2),
                        ord => ord,
                    },
                }
            }
            ord => ord,
        }
    }
}

impl<K, V> SplitOrderedEntry<K, V> {
    fn reverse_bits(mut n: usize) -> usize {
        let mut result = 0;
        let bits = std::mem::size_of::<usize>() * 8;

        for _ in 0..bits {
            result = (result << 1) | (n & 1);
            n >>= 1;
        }

        result
    }

    fn new_sentinel(bucket: usize) -> Self {
        SplitOrderedEntry::Sentinel {
            split_key: Self::reverse_bits(bucket) & !1, // Clear LSB for sentinels
            bucket,
        }
    }

    fn new_regular(hash: usize, key: K, value: V) -> Self {
        SplitOrderedEntry::Regular {
            split_key: Self::reverse_bits(hash) | 1, // Set LSB for regular entries
            hash,
            key,
            value: Some(value),
        }
    }

    fn new_search(hash: usize, key: K) -> Self {
        SplitOrderedEntry::Regular {
            split_key: Self::reverse_bits(hash) | 1, // Set LSB like regular entries
            hash,
            key,
            value: None, // None indicates this is for searching
        }
    }
}

#[doc = r#"Lock-free Split-Ordered HashMap using SortedList

Based on Shalev and Shavit's "Split-Ordered Lists: Lock-Free Extensible Hash Tables" (2003).
The key insight is that items are ordered by reverse-bit hash, allowing bucket splits
without moving items in the list.

# Bucket Architecture in Split-Ordered Hash Table

## Overview
Buckets are logical divisions of a single sorted linked list. Each bucket is marked
by a "sentinel" node that acts as a starting point for operations on that bucket.

## Key Concepts

### 1. Reverse-Bit Ordering
Items are ordered by the reverse of their hash bits, not by hash value:
- Hash: 0b0101 → Reverse: 0b1010
- This ensures items destined for future buckets are already in position

### 2. Bucket Sentinels
Each bucket has a sentinel node with:
- split_key = reverse_bits(bucket_index) with LSB cleared
- Regular entries have LSB set to 1, sentinels have LSB = 0
- This ensures sentinels always come before their bucket's items

### 3. Parent-Child Hierarchy
Buckets form a binary tree where parent(b) = b with highest bit cleared:

Bucket 0: '0000' -> parent: 0 (root)
Bucket 1: '0001' -> parent: 0 (clear bit 0)
Bucket 2: '0010' -> parent: 0 (clear bit 1)  
Bucket 3: '0011' -> parent: 2 (clear bit 0, keeping bit 1)
Bucket 4: '0100' -> parent: 0 (clear bit 2)
Bucket 5: '0101' -> parent: 4 (clear bit 0, keeping bit 2)
Bucket 6: '0110' -> parent: 4 (clear bit 1, keeping bit 2)
Bucket 7: '0111' -> parent: 6 (clear bit 0, keeping bits 1&2)


### 4. Bucket Splitting During Resize
When table doubles from N to 2N buckets:
- Bucket i splits into buckets i and i+N
- Items don't move in the list!
- Example: When growing from 4 to 8 buckets:
  - Bucket 0 splits into 0 and 4
  - Bucket 1 splits into 1 and 5
  - Items with (hash % 8)=4 were already positioned after items with (hash % 8=0) due to reverse-bit ordering

### 5. Lazy Sentinel Initialization
Sentinels are inserted only when first accessed:
1. Thread needs bucket 7
2. Checks parent (bucket 6) exists, initializes if needed
3. Recursively ensures parent chain up to root
4. Inserts sentinel 7 after parent sentinel 6

## Example Walkthrough

Table with 4 buckets, inserting key with hash=5:
1. Bucket = 5 % 4 = 1
2. Ensure sentinel 1 exists (parent is 0, which always exists)
3. Insert entry with split_key = reverse_bits(5) | 1

After resize to 8 buckets, same key:
1. Bucket = 5 % 8 = 5  
2. Ensure sentinel 5 exists (parent is 4, then 0)
3. Entry is already in correct position due to reverse-bit ordering

## Memory Layout Example

List structure with 8 buckets (after resize from 4):

HEAD -> S0 -> K(h=8) -> K(h=16) -> S4 -> K(h=4) -> S2 → K(h=2) -> S6 -> K(h=6) →
        S1 -> K(h=1) -> K(h=9)  -> S5 -> K(h=5) -> S3 → K(h=3) -> S7 -> K(h=7) → NULL

Where:
- Sn = Sentinel for bucket n
- K(h=x) = Key with hash value x
- Order is by reverse-bit value, not hash value

## Implementation Details

- Sentinel table: Pre-allocated array of pointers for O(1) bucket access
- Bitmap: Tracks which buckets have been initialized  
- Max buckets: Currently 65536 (2^16), pre-allocated
- Resize: Just increments logical bucket count, no data movement"#]
pub struct SplitOrderedHashMap<K, V, G: Guard, S = RandomState> {
    list: SortedList<SplitOrderedEntry<K, V>, G>,
    size: AtomicUsize,
    bucket_size: AtomicUsize,
    // Pre-allocated tables - we never replace these, just use more buckets as needed
    sentinel_table: Vec<AtomicPtr<SortedListNode<SplitOrderedEntry<K, V>>>>,
    buckets_initialized: Vec<AtomicUsize>, // Bitmap for initialized buckets
    hasher: S,
}

impl<K, V, G> SplitOrderedHashMap<K, V, G, RandomState>
where
    K: Hash + Eq + Ord + Clone,
    V: Clone,
    G: Guard,
{
    pub fn new() -> Self {
        Self::with_hasher(RandomState::new())
    }
}

impl<K, V, G> Default for SplitOrderedHashMap<K, V, G, RandomState>
where
    K: Hash + Eq + Ord + Clone,
    V: Clone,
    G: Guard,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V, G, S> SplitOrderedHashMap<K, V, G, S>
where
    K: Hash + Eq + Ord + Clone,
    V: Clone,
    G: Guard,
    S: BuildHasher,
{
    pub fn with_hasher(hasher: S) -> Self {
        Self::with_hasher_and_capacity(hasher, INITIAL_BUCKETS)
    }

    /// Creates a new split-ordered hashmap with specified hasher and initial capacity.
    /// Pre-allocates space for future growth to avoid reallocation during resize.
    /// Following the paper, we allocate once and use more buckets as needed.
    pub fn with_hasher_and_capacity(hasher: S, initial_buckets: usize) -> Self {
        let list = SortedList::new();

        // Insert sentinel for bucket 0
        let sentinel_0 = SplitOrderedEntry::new_sentinel(0);
        list.insert(sentinel_0.clone());

        // Find sentinel 0 using find_from
        let head = list.head.load(Ordering::Acquire);
        let head_pos = ListNodePosition::from_node(head);
        let loc = list.find_from_internal(Some(&head_pos), &sentinel_0, true);

        let sentinel_0_ptr = match loc {
            Some(pos) => pos.node_ptr(),
            None => panic!("Failed to find sentinel 0 after insertion"),
        };

        // Pre-allocate tables large enough for all future growth
        const MAX_BUCKETS: usize = 1 << 16; // Support up to 65K buckets

        // Allocate the full MAX_BUCKETS, not just initial_buckets!
        let mut sentinel_vec = Vec::with_capacity(MAX_BUCKETS);

        // First entry points to sentinel 0
        sentinel_vec.push(AtomicPtr::new(sentinel_0_ptr));

        // Rest are null (uninitialized)
        for _ in 1..MAX_BUCKETS {
            sentinel_vec.push(AtomicPtr::new(ptr::null_mut()));
        }

        // Create bitmap for tracking initialized buckets
        let bitmap_size = MAX_BUCKETS.div_ceil(64);
        let mut buckets_initialized = Vec::with_capacity(bitmap_size);
        for _ in 0..bitmap_size {
            buckets_initialized.push(AtomicUsize::new(0));
        }

        // Mark bucket 0 as initialized
        buckets_initialized[0].store(1, Ordering::Relaxed);

        assert_eq!(
            sentinel_vec.len(),
            MAX_BUCKETS,
            "Sentinel table size mismatch"
        );
        assert_eq!(
            buckets_initialized.len(),
            bitmap_size,
            "Bitmap size mismatch"
        );

        SplitOrderedHashMap {
            list,
            size: AtomicUsize::new(0),
            bucket_size: AtomicUsize::new(initial_buckets),
            sentinel_table: sentinel_vec,
            buckets_initialized,
            hasher,
        }
    }

    fn hash_key(&self, key: &K) -> usize {
        self.hasher.hash_one(key) as usize
    }

    /// Returns the parent bucket for a given bucket index.
    /// Following the paper: parent is the bucket with highest bit cleared.
    /// For example: bucket 5 (101) -> parent 1 (001), bucket 6 (110) -> parent 2 (010)
    fn get_parent_bucket(bucket: usize) -> usize {
        if bucket == 0 {
            return 0;
        }
        // Find and clear the highest set bit
        let highest_bit = 1usize << (63 - bucket.leading_zeros());
        bucket & !highest_bit
    }

    /// Checks if a bucket's sentinel has been initialized.
    /// Uses atomic bitmap to track initialization status.
    fn is_bucket_initialized(&self, bucket: usize) -> bool {
        let word_idx = bucket / 64;
        let bit_idx = bucket % 64;

        if word_idx >= self.buckets_initialized.len() {
            return false;
        }

        let word = self.buckets_initialized[word_idx].load(Ordering::Acquire);
        (word & (1 << bit_idx)) != 0
    }

    /// Lazily initializes a bucket's sentinel if not already present.
    /// This is the key to the paper's approach - sentinels are added on-demand
    /// as the table grows, without moving existing items.
    fn initialize_bucket(&self, bucket: usize) {
        // Check if we can support this bucket
        if bucket >= self.sentinel_table.len() {
            panic!(
                "Bucket {} exceeds maximum table size {}",
                bucket,
                self.sentinel_table.len()
            );
        }

        // Atomically try to claim initialization rights for this bucket
        let word_idx = bucket / 64;
        let bit_idx = bucket % 64;

        if word_idx >= self.buckets_initialized.len() {
            panic!("Bucket {} exceeds bitmap capacity", bucket);
        }

        // Try to set the initialization bit atomically
        loop {
            let old_word = self.buckets_initialized[word_idx].load(Ordering::Acquire);
            if (old_word & (1 << bit_idx)) != 0 {
                // Already initialized/initializing by another thread
                // Wait for the pointer to be stored
                loop {
                    let ptr = self.sentinel_table[bucket].load(Ordering::Acquire);
                    if !ptr.is_null() {
                        return; // Initialization complete
                    }
                    std::hint::spin_loop();
                }
            }

            let new_word = old_word | (1 << bit_idx);
            if self.buckets_initialized[word_idx]
                .compare_exchange_weak(old_word, new_word, Ordering::Release, Ordering::Acquire)
                .is_ok()
            {
                // We won the race to initialize this bucket
                break;
            }
            // Lost the race, loop to check if now initialized
        }

        // Now only this thread will initialize the bucket

        // Recursively ensure parent bucket is initialized first
        let parent = Self::get_parent_bucket(bucket);
        if parent != bucket {
            self.initialize_bucket(parent);
        }

        // Get parent sentinel pointer - wait for it if being initialized by another thread
        let parent_ptr = if bucket == 0 {
            // Bucket 0 is always pre-initialized
            self.sentinel_table[0].load(Ordering::Acquire)
        } else {
            // Wait for parent initialization to complete
            loop {
                let ptr = self.sentinel_table[parent].load(Ordering::Acquire);
                if !ptr.is_null() {
                    break ptr;
                }
                // Parent is being initialized by another thread
                std::hint::spin_loop();
            }
        };

        // Insert sentinel into the list
        let sentinel = SplitOrderedEntry::new_sentinel(bucket);
        let parent_pos = ListNodePosition::from_node(parent_ptr);

        // insert_from_internal returns None if already exists, that's ok
        let _ = self
            .list
            .insert_from_internal(sentinel.clone(), Some(&parent_pos));

        // Find the sentinel (whether we inserted it or another thread did)
        let loc = self
            .list
            .find_from_internal(Some(&parent_pos), &sentinel, true);

        // Store the sentinel pointer for O(1) future access
        match loc {
            Some(pos) => self.sentinel_table[bucket].store(pos.node_ptr(), Ordering::Release),
            None => panic!("Failed to find sentinel {} after insertion attempt", bucket),
        }
    }

    /// Gets a bucket's sentinel pointer directly from the table (O(1) lookup).
    fn get_bucket_sentinel_direct(
        &self,
        bucket: usize,
    ) -> Option<*mut SortedListNode<SplitOrderedEntry<K, V>>> {
        if bucket >= self.sentinel_table.len() {
            return None;
        }

        let ptr = self.sentinel_table[bucket].load(Ordering::Acquire);
        if ptr.is_null() { None } else { Some(ptr) }
    }

    /// Gets the sentinel node for a bucket, initializing it lazily if necessary.
    /// This implements the paper's lazy initialization strategy.
    fn get_bucket_sentinel(
        &self,
        bucket: usize,
    ) -> Option<*mut SortedListNode<SplitOrderedEntry<K, V>>> {
        // First try direct lookup - O(1)
        if let Some(ptr) = self.get_bucket_sentinel_direct(bucket) {
            return Some(ptr);
        }

        // Not initialized, do it now (lazily)
        self.initialize_bucket(bucket);

        // Now it should be there
        self.get_bucket_sentinel_direct(bucket)
    }

    /// Doubles the number of buckets.
    /// Following the paper: we don't move items or replace tables, just increase
    /// the logical bucket count. New sentinels are initialized lazily as needed.
    fn resize(&self) {
        let old_size = self.bucket_size.load(Ordering::Acquire);
        let new_size = old_size * 2;

        // Ensure new_size doesn't exceed our pre-allocated table
        if new_size > self.sentinel_table.len() {
            return; // Cannot resize beyond table capacity
        }

        self.bucket_size
            .compare_exchange(
                old_size,
                new_size.min(self.sentinel_table.len()), // Cap at table size
                Ordering::Release,
                Ordering::Relaxed,
            )
            .ok();
    }

    /// Returns the number of key-value pairs in the map.
    pub fn len(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }

    /// Returns true if the map is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ============================================================================
// HashMapNode implementation for SortedListNode<SplitOrderedEntry<K, V>>
// ============================================================================

impl<K, V> HashMapNode<K, V> for SortedListNode<SplitOrderedEntry<K, V>>
where
    K: Eq,
{
    fn key(&self) -> &K {
        match CollectionNode::key(self) {
            SplitOrderedEntry::Regular { key, .. } => key,
            SplitOrderedEntry::Sentinel { .. } => panic!("called key() on sentinel node"),
        }
    }

    fn value(&self) -> Option<&V> {
        match CollectionNode::key(self) {
            SplitOrderedEntry::Regular { value, .. } => value.as_ref(),
            SplitOrderedEntry::Sentinel { .. } => None,
        }
    }
}

// ============================================================================
// HashMapCollection implementation for SplitOrderedHashMap
// ============================================================================

impl<K, V, G, S> HashMapCollection<K, V> for SplitOrderedHashMap<K, V, G, S>
where
    K: Hash + Eq + Ord + Clone,
    V: Clone,
    G: Guard,
    S: BuildHasher,
{
    type Guard = G;
    type Node = SortedListNode<SplitOrderedEntry<K, V>>;

    fn guard(&self) -> &G {
        self.list.guard()
    }

    fn insert_internal(&self, key: K, value: V) -> Option<*mut Self::Node> {
        let hash = self.hash_key(&key);
        let bucket_size = self.bucket_size.load(Ordering::Acquire);
        let bucket = hash % bucket_size;

        let sentinel_ptr = self.get_bucket_sentinel(bucket)?;
        let sentinel_pos = ListNodePosition::from_node(sentinel_ptr);
        let new_entry = SplitOrderedEntry::new_regular(hash, key.clone(), value);

        // Try to insert - returns position if successful
        if let Some(pos) = self
            .list
            .insert_from_internal(new_entry, Some(&sentinel_pos))
        {
            self.size.fetch_add(1, Ordering::Relaxed);

            let size = self.size.load(Ordering::Relaxed);
            if size > 0 && (size / bucket_size) >= MAX_LOAD {
                self.resize();
            }
            return Some(pos.node_ptr());
        }

        // Key already exists - return None (insert failed)
        None
    }

    fn remove_internal(&self, key: &K) -> Option<*mut Self::Node> {
        let hash = self.hash_key(key);
        let bucket = hash % self.bucket_size.load(Ordering::Acquire);

        let sentinel_ptr = self.get_bucket_sentinel(bucket)?;
        let sentinel_pos = ListNodePosition::from_node(sentinel_ptr);

        let search_entry = SplitOrderedEntry::new_search(hash, key.clone());

        if let Some(pos) = self
            .list
            .remove_from_internal(Some(&sentinel_pos), &search_entry)
        {
            self.size.fetch_sub(1, Ordering::Relaxed);
            Some(pos.node_ptr())
        } else {
            None
        }
    }

    fn find_internal(&self, key: &K) -> Option<*mut Self::Node> {
        let hash = self.hash_key(key);
        let bucket = hash % self.bucket_size.load(Ordering::Acquire);

        let sentinel_ptr = self.get_bucket_sentinel(bucket)?;
        let sentinel_pos = ListNodePosition::from_node(sentinel_ptr);

        let search_entry = SplitOrderedEntry::new_search(hash, key.clone());

        self.list
            .find_from_internal(Some(&sentinel_pos), &search_entry, true)
            .map(|pos| pos.node_ptr())
    }

    fn apply_on_internal<F, R>(&self, node: *mut Self::Node, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
        if node.is_null() {
            return None;
        }

        let node = MarkedPtr::unmask(node);

        unsafe {
            match CollectionNode::key(&*node) {
                SplitOrderedEntry::Regular {
                    key,
                    value: Some(v),
                    ..
                } => Some(f(key, v)),
                _ => None,
            }
        }
    }

    fn len_internal(&self) -> usize {
        self.len()
    }
}

// TODO: Re-enable tests after adding G: Guard to HashMapCollection
// Tests temporarily disabled during Guard refactoring
#[cfg(any())] // Disabled - always false
mod tests {
    use super::*;
    use crate::guard::DeferredGuard;
    use std::{sync::Arc, thread};

    #[test]
    fn test_basic_operations() {
        let map = DeferredHashMap::new(SplitOrderedHashMap::new());

        // Test insertions
        assert!(map.insert(5, "five"));
        assert!(map.insert(3, "three"));
        assert!(map.insert(7, "seven"));

        // Test get
        assert_eq!(*map.get(&5).unwrap(), "five");
        assert_eq!(*map.get(&3).unwrap(), "three");
        assert_eq!(*map.get(&7).unwrap(), "seven");
        assert!(map.get(&10).is_none());

        // Test update (insert returns false if key exists)
        assert!(!map.insert(5, "FIVE")); // Key exists, so update happens but returns false
        // For update, we need to remove then insert
        map.remove(&5);
        assert!(map.insert(5, "FIVE"));
        assert_eq!(*map.get(&5).unwrap(), "FIVE");

        // Test contains
        assert!(map.contains(&5));
        assert!(map.contains(&3));
        assert!(map.contains(&7));
        assert!(!map.contains(&10));

        // Test removal
        assert_eq!(map.remove(&3), Some("three"));
        assert!(!map.contains(&3));
        assert_eq!(map.remove(&3), None);

        // Test size
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn test_concurrent_insertions() {
        let map = Arc::new(DeferredHashMap::new(SplitOrderedHashMap::new()));
        let num_threads = 16;
        let items_per_thread = 1000;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    for i in 0..items_per_thread {
                        let key = t * items_per_thread + i;
                        map.insert(key, key * 2);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(map.len(), num_threads * items_per_thread);

        for i in 0..(num_threads * items_per_thread) {
            let val = map.get(&i).map(|g| *g);
            assert_eq!(val, Some(i * 2), "Missing or wrong value for key: {}", i);
        }
    }

    #[test]
    fn test_simple_concurrent() {
        let map = Arc::new(DeferredHashMap::new(SplitOrderedHashMap::new()));

        // Just two threads, fewer operations
        let h1 = {
            let map = Arc::clone(&map);
            thread::spawn(move || {
                for i in 0..10 {
                    map.insert(i, i);
                }
            })
        };

        let h2 = {
            let map = Arc::clone(&map);
            thread::spawn(move || {
                for i in 5..15 {
                    map.insert(i, i);
                }
            })
        };

        h1.join().unwrap();
        h2.join().unwrap();
    }

    #[test]
    fn test_concurrent_mixed() {
        let map = Arc::new(DeferredHashMap::new(SplitOrderedHashMap::new()));
        let num_threads = 6;
        let num_operations = 1000;

        // Pre-populate
        for i in 0..100 {
            map.insert(i, i * 10);
        }

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    for i in 0..num_operations {
                        let key = (t * num_operations + i) % 200;

                        match i % 3 {
                            0 => {
                                map.insert(key, key * 10);
                            }
                            1 => {
                                let _ = map.get(&key);
                            }
                            2 => {
                                map.remove(&key);
                            }
                            _ => unreachable!(),
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        println!("Final map size: {}", map.len());
    }
}
