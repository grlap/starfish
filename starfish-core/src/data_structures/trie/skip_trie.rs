use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// Import the split-ordered hash table
use crate::data_structures::split_ordered_hash_table::SplitOrderedHashMap;

thread_local! {
    /// Simple thread-local PRNG using Linear Congruential Generator.
    ///
    static RNG_STATE: Cell<u64> = Cell::new({
        // Seed with current time and thread info for better randomness
        let time_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        // Mix in thread ID for better distribution across threads.
        //
        let thread_id = std::thread::current().id();
        let mut hasher = DefaultHasher::new();
        thread_id.hash(&mut hasher);
        let thread_seed = hasher.finish();

        time_seed.wrapping_add(thread_seed).wrapping_add(1)
    });
}

fn next_random() -> u64 {
    RNG_STATE.with(|state| {
        let current = state.get();
        // LCG parameters from Numerical Recipes
        let next = current.wrapping_mul(1664525).wrapping_add(1013904223);
        state.set(next);
        next
    })
}

fn random_bool() -> bool {
    (next_random() & 1) == 1
}

/// Maximum key universe size (2^30).
///
const MAX_U: usize = 1 << 30;

/// Calculate log log u for skiplist height.
///
fn log_log_u(u: usize) -> usize {
    if u <= 2 {
        return 1;
    }
    let log_u = (usize::BITS - u.leading_zeros() - 1) as usize;
    std::cmp::max(1, (usize::BITS - log_u.leading_zeros() - 1) as usize)
}

/// Node in the truncated skiplist
#[repr(C)]
struct SkipListNode<K, V> {
    key: K,
    value: V,
    height: usize,
    /// Next pointers for each level
    next: Box<[AtomicPtr<SkipListNode<K, V>>]>,
    /// Back pointer for recovery
    back: AtomicPtr<SkipListNode<K, V>>,
    /// Marked for deletion
    marked: AtomicBool,
    /// Ready flag for top-level nodes
    ready: AtomicBool,
    /// Previous pointer for top level
    prev: AtomicPtr<SkipListNode<K, V>>,
}

impl<K: Clone, V: Clone> SkipListNode<K, V> {
    fn new(key: K, value: V, height: usize) -> Box<Self> {
        let mut next = Vec::with_capacity(height + 1);
        for _ in 0..=height {
            next.push(AtomicPtr::new(ptr::null_mut()));
        }

        Box::new(SkipListNode {
            key,
            value,
            height,
            next: next.into_boxed_slice(),
            back: AtomicPtr::new(ptr::null_mut()),
            marked: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            prev: AtomicPtr::new(ptr::null_mut()),
        })
    }

    fn new_sentinel(height: usize) -> Box<Self>
    where
        K: Default,
        V: Default,
    {
        let mut next = Vec::with_capacity(height + 1);
        for _ in 0..=height {
            next.push(AtomicPtr::new(ptr::null_mut()));
        }

        Box::new(SkipListNode {
            key: K::default(),
            value: V::default(),
            height,
            next: next.into_boxed_slice(),
            back: AtomicPtr::new(ptr::null_mut()),
            marked: AtomicBool::new(false),
            ready: AtomicBool::new(true), // Sentinels are always ready
            prev: AtomicPtr::new(ptr::null_mut()),
        })
    }
}

/// Truncated skiplist component
struct TruncatedSkipList<K, V> {
    head: AtomicPtr<SkipListNode<K, V>>,
    max_height: usize,
    key_universe_size: usize,
}

impl<K: Clone + Ord + Hash + Default, V: Clone + Default> TruncatedSkipList<K, V> {
    fn new(key_universe_size: usize) -> Self {
        let max_height = log_log_u(key_universe_size);
        let head = SkipListNode::new_sentinel(max_height);
        let head_ptr = Box::into_raw(head);

        TruncatedSkipList {
            head: AtomicPtr::new(head_ptr),
            max_height,
            key_universe_size,
        }
    }

    /// Generate pseudo-random height for a new node based on key hash
    fn random_height(&self, key: &K) -> usize {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = hasher.finish();

        let mut height = 0;
        let mut bits = hash;

        // Use geometric distribution: flip coins until we get heads or reach max height
        while height < self.max_height && (bits & 1) == 1 {
            height += 1;
            bits >>= 1;
        }

        height
    }

    /// Generate truly random height (fallback method)
    fn random_height_fallback(&self) -> usize {
        let mut height = 0;
        while height < self.max_height && random_bool() {
            height += 1;
        }
        height
    }

    /// Find position for key at given level
    fn find_position(
        &self,
        key: &K,
        level: usize,
    ) -> (*mut SkipListNode<K, V>, *mut SkipListNode<K, V>) {
        unsafe {
            let head = self.head.load(Ordering::Acquire);
            let mut prev = head;
            let mut curr = (*head).next[level].load(Ordering::Acquire);

            while !curr.is_null() {
                if (*curr).marked.load(Ordering::Acquire) {
                    // Skip marked nodes
                    let next = (*curr).next[level].load(Ordering::Acquire);
                    if (*prev).next[level]
                        .compare_exchange(curr, next, Ordering::Release, Ordering::Relaxed)
                        .is_ok()
                    {
                        curr = next;
                        continue;
                    }
                }

                if (*curr).key >= *key {
                    break;
                }

                prev = curr;
                curr = (*curr).next[level].load(Ordering::Acquire);
            }

            (prev, curr)
        }
    }

    /// Insert a key-value pair
    fn insert(&self, key: K, value: V) -> Option<*mut SkipListNode<K, V>> {
        let height = self.random_height(&key);
        let new_node = SkipListNode::new(key.clone(), value, height);
        let new_ptr = Box::into_raw(new_node);

        // Insert from bottom up
        for level in 0..=height {
            loop {
                let (prev, curr) = self.find_position(&key, level);

                unsafe {
                    // Check if key already exists
                    if !curr.is_null()
                        && (*curr).key == key
                        && !(*curr).marked.load(Ordering::Acquire)
                        && level == 0
                    {
                        // Clean up and return null for duplicate
                        drop(Box::from_raw(new_ptr));
                        return None;
                    }

                    (*new_ptr).next[level].store(curr, Ordering::Release);

                    if (*prev).next[level]
                        .compare_exchange(curr, new_ptr, Ordering::Release, Ordering::Relaxed)
                        .is_ok()
                    {
                        break;
                    }
                }
            }
        }

        // If reached top level, mark as ready after setting up prev pointer
        if height == self.max_height {
            self.setup_prev_pointer(new_ptr);
            unsafe {
                (*new_ptr).ready.store(true, Ordering::Release);
            }
        } else {
            unsafe {
                (*new_ptr).ready.store(true, Ordering::Release);
            }
        }

        Some(new_ptr)
    }

    /// Setup prev pointer for top-level nodes
    fn setup_prev_pointer(&self, node: *mut SkipListNode<K, V>) {
        unsafe {
            let (prev, _) = self.find_position(&(*node).key, self.max_height);
            (*node).prev.store(prev, Ordering::Release);
        }
    }

    /// Find predecessor of a key (largest key ≤ target)
    fn find_predecessor(&self, key: &K) -> Option<*mut SkipListNode<K, V>> {
        unsafe {
            let head = self.head.load(Ordering::Acquire);
            let mut curr = head;

            // Start from top level and work down, carrying position between levels
            for level in (0..=self.max_height).rev() {
                loop {
                    let next = (*curr).next[level].load(Ordering::Acquire);

                    // Stop if we've reached the end or found a key too large
                    if next.is_null() || (*next).key > *key {
                        break;
                    }

                    if !(*next).marked.load(Ordering::Acquire) {
                        curr = next;
                    } else {
                        // Try to unlink marked node
                        let next_next = (*next).next[level].load(Ordering::Acquire);
                        (*curr).next[level]
                            .compare_exchange(next, next_next, Ordering::Release, Ordering::Relaxed)
                            .ok();
                    }
                }
                // Continue from current position at the next level down
            }

            // Check if we found a valid predecessor
            if curr == head { None } else { Some(curr) }
        }
    }

    /// Find strict predecessor of a key (largest key < target)
    fn find_strict_predecessor(&self, key: &K) -> Option<*mut SkipListNode<K, V>> {
        unsafe {
            let head = self.head.load(Ordering::Acquire);
            let mut curr = head;

            // Start from top level and work down, carrying position between levels
            for level in (0..=self.max_height).rev() {
                loop {
                    let next = (*curr).next[level].load(Ordering::Acquire);

                    // Stop if we've reached the end or found a key >= target (note: >= instead of >)
                    if next.is_null() || (*next).key >= *key {
                        break;
                    }

                    if !(*next).marked.load(Ordering::Acquire) {
                        curr = next;
                    } else {
                        // Try to unlink marked node
                        let next_next = (*next).next[level].load(Ordering::Acquire);
                        (*curr).next[level]
                            .compare_exchange(next, next_next, Ordering::Release, Ordering::Relaxed)
                            .ok();
                    }
                }
                // Continue from current position at the next level down
            }

            // Check if we found a valid strict predecessor
            if curr == head { None } else { Some(curr) }
        }
    }

    /// Delete a key
    /// Delete a key
    /// Delete a key
    fn delete(&self, key: &K) -> bool {
        // Find the node first
        let (_, curr) = self.find_position(key, 0);

        if curr.is_null() {
            return false;
        }

        unsafe {
            if (*curr).key != *key || (*curr).marked.load(Ordering::Acquire) {
                return false;
            }

            // Mark for deletion atomically to prevent double deletion
            if (*curr)
                .marked
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                return false; // Already marked by another thread
            }

            // Unlink from all levels
            for level in (0..=(*curr).height).rev() {
                loop {
                    let (prev, _) = self.find_position(key, level);

                    // Check if prev actually points to our node at this level
                    let prev_next = (*prev).next[level].load(Ordering::Acquire);
                    if prev_next != curr {
                        // Node has already been unlinked at this level
                        break;
                    }

                    let next = (*curr).next[level].load(Ordering::Acquire);

                    if (*prev).next[level]
                        .compare_exchange(curr, next, Ordering::Release, Ordering::Relaxed)
                        .is_ok()
                    {
                        break;
                    } else {
                        // CAS failed, check again if still linked
                        let current_prev_next = (*prev).next[level].load(Ordering::Acquire);
                        if current_prev_next != curr {
                            // Node was unlinked by someone else, we're done at this level
                            break;
                        }
                        // Otherwise retry
                    }
                }
            }

            true
        }
    }

    /// Get all top-level nodes (for x-fast trie)
    fn get_top_level_nodes(&self) -> Vec<*mut SkipListNode<K, V>> {
        let mut nodes = Vec::new();
        unsafe {
            let head = self.head.load(Ordering::Acquire);
            let mut curr = (*head).next[self.max_height].load(Ordering::Acquire);

            while !curr.is_null() {
                if !(*curr).marked.load(Ordering::Acquire) && (*curr).ready.load(Ordering::Acquire)
                {
                    nodes.push(curr);
                }
                curr = (*curr).next[self.max_height].load(Ordering::Acquire);
            }
        }
        nodes
    }
}

/// X-Fast Trie node
struct XFastTrieNode<K, V> {
    /// Pointers to largest in 0-subtree and smallest in 1-subtree
    pointers: [AtomicPtr<SkipListNode<K, V>>; 2],
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> XFastTrieNode<K, V> {
    fn new() -> Self {
        XFastTrieNode {
            pointers: [
                AtomicPtr::new(ptr::null_mut()),
                AtomicPtr::new(ptr::null_mut()),
            ],
            _phantom: PhantomData,
        }
    }

    fn is_empty(&self) -> bool {
        self.pointers[0].load(Ordering::Acquire).is_null()
            && self.pointers[1].load(Ordering::Acquire).is_null()
    }
}

/// X-Fast Trie implementation
struct XFastTrie<K, V> {
    prefixes: SplitOrderedHashTable<String, XFastTrieNode<K, V>>,
    key_bits: usize,
}

impl<K: Clone + Ord + Hash, V: Clone> XFastTrie<K, V> {
    fn new(key_universe_size: usize) -> Self {
        let key_bits = (usize::BITS - key_universe_size.leading_zeros()) as usize;
        XFastTrie {
            prefixes: SplitOrderedHashTable::new(),
            key_bits,
        }
    }

    /// Convert key to bit string
    fn key_to_bits(&self, key: &K) -> String {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = hasher.finish() as usize;
        format!("{:0width$b}", hash, width = self.key_bits)
    }

    /// Get prefix of given length
    fn get_prefix(&self, key_bits: &str, len: usize) -> String {
        if len == 0 {
            String::new()
        } else {
            key_bits[..std::cmp::min(len, key_bits.len())].to_string()
        }
    }

    /// Find longest common prefix using binary search
    fn find_longest_common_prefix(&self, key: &K) -> Option<*mut SkipListNode<K, V>> {
        let key_bits = self.key_to_bits(key);
        let mut best_node = ptr::null_mut();
        let mut start = 0;
        let mut size = self.key_bits / 2;

        while size > 0 {
            let prefix = self.get_prefix(&key_bits, start + size);

            if let Some(trie_node) = self.prefixes.find(&prefix) {
                let direction = if start + size < key_bits.len() {
                    key_bits.chars().nth(start + size).unwrap() as usize - '0' as usize
                } else {
                    0
                };

                let candidate = trie_node.pointers[direction].load(Ordering::Acquire);
                if !candidate.is_null() {
                    best_node = candidate;
                    start += size;
                }
            }

            size /= 2;
        }

        if best_node.is_null() {
            None
        } else {
            Some(best_node)
        }
    }

    /// Insert a top-level skiplist node into the trie
    fn insert_node(&self, node: *mut SkipListNode<K, V>) -> bool {
        unsafe {
            let key_bits = self.key_to_bits(&(*node).key);

            // Insert all prefixes from longest to shortest
            for i in (0..=self.key_bits).rev() {
                let prefix = self.get_prefix(&key_bits, i);

                if let Some(trie_node) = self.prefixes.find(&prefix) {
                    let direction = if i < key_bits.len() {
                        key_bits.chars().nth(i).unwrap() as usize - '0' as usize
                    } else {
                        0
                    };

                    // Update pointer if this node is better
                    let current = trie_node.pointers[direction].load(Ordering::Acquire);
                    let should_update = if current.is_null() {
                        true
                    } else if direction == 0 {
                        // For 0-subtree, we want the largest key
                        (*node).key >= (*current).key
                    } else {
                        // For 1-subtree, we want the smallest key
                        (*node).key <= (*current).key
                    };

                    if should_update {
                        trie_node.pointers[direction].store(node, Ordering::Release);
                    }
                } else {
                    // Create new trie node
                    let new_trie_node = XFastTrieNode::new();
                    let direction = if i < key_bits.len() {
                        key_bits.chars().nth(i).unwrap() as usize - '0' as usize
                    } else {
                        0
                    };
                    new_trie_node.pointers[direction].store(node, Ordering::Release);

                    if !self.prefixes.insert(prefix, new_trie_node) {
                        // Someone else inserted, try again
                        continue;
                    }
                }
            }

            true
        }
    }

    /// Remove a node from the trie
    fn remove_node(&self, node: *mut SkipListNode<K, V>) {
        unsafe {
            let key_bits = self.key_to_bits(&(*node).key);

            for i in 0..=self.key_bits {
                let prefix = self.get_prefix(&key_bits, i);

                if let Some(trie_node) = self.prefixes.find(&prefix) {
                    let direction = if i < key_bits.len() {
                        key_bits.chars().nth(i).unwrap() as usize - '0' as usize
                    } else {
                        0
                    };

                    let current = trie_node.pointers[direction].load(Ordering::Acquire);
                    if current == node {
                        trie_node.pointers[direction].store(ptr::null_mut(), Ordering::Release);

                        // If trie node is now empty, remove it
                        if trie_node.is_empty() {
                            self.prefixes.delete(&prefix);
                        }
                    }
                }
            }
        }
    }
}

/// Main SkipTrie data structure
pub struct SkipTrie<K, V> {
    skiplist: TruncatedSkipList<K, V>,
    x_fast_trie: XFastTrie<K, V>,
    count: AtomicUsize,
    key_universe_size: usize,
}

impl<K: Clone + Ord + Hash + Default, V: Clone + Default> SkipTrie<K, V> {
    /// Create a new SkipTrie
    pub fn new() -> Self {
        Self::with_universe_size(MAX_U)
    }

    /// Create a new SkipTrie with specified key universe size
    pub fn with_universe_size(key_universe_size: usize) -> Self {
        SkipTrie {
            skiplist: TruncatedSkipList::new(key_universe_size),
            x_fast_trie: XFastTrie::new(key_universe_size),
            count: AtomicUsize::new(0),
            key_universe_size,
        }
    }

    /// Insert a key-value pair
    pub fn insert(&self, key: K, value: V) -> bool {
        if let Some(node) = self.skiplist.insert(key.clone(), value) {
            self.count.fetch_add(1, Ordering::AcqRel);

            // If node reached top level, insert into x-fast trie
            unsafe {
                if (*node).height == self.skiplist.max_height {
                    self.x_fast_trie.insert_node(node);
                }
            }

            true
        } else {
            false
        }
    }

    /// Find the predecessor of a key (largest key ≤ target)
    pub fn predecessor(&self, key: &K) -> Option<(K, V)> {
        // Use skiplist search directly - it's more reliable
        if let Some(pred) = self.skiplist.find_predecessor(key) {
            unsafe {
                if !pred.is_null() && !(*pred).marked.load(Ordering::Acquire) {
                    return Some(((*pred).key.clone(), (*pred).value.clone()));
                }
            }
        }

        None
    }

    /// Find the strict predecessor of a key (largest key < target)
    pub fn strict_predecessor(&self, key: &K) -> Option<(K, V)> {
        if let Some(pred) = self.skiplist.find_strict_predecessor(key) {
            unsafe {
                if !pred.is_null() && !(*pred).marked.load(Ordering::Acquire) {
                    return Some(((*pred).key.clone(), (*pred).value.clone()));
                }
            }
        }
        None
    }

    /// Find a key in the trie
    pub fn find(&self, key: &K) -> Option<V> {
        if let Some((pred_key, pred_value)) = self.predecessor(key)
            && pred_key == *key
        {
            return Some(pred_value);
        }
        None
    }

    /// Delete a key from the trie
    pub fn delete(&self, key: &K) -> bool {
        // Check if it's a top-level node first
        let top_level_nodes = self.skiplist.get_top_level_nodes();
        let mut target_node = ptr::null_mut();

        unsafe {
            for &node in &top_level_nodes {
                if (*node).key == *key {
                    target_node = node;
                    break;
                }
            }
        }

        if self.skiplist.delete(key) {
            self.count.fetch_sub(1, Ordering::AcqRel);

            // If it was a top-level node, remove from x-fast trie
            if !target_node.is_null() {
                self.x_fast_trie.remove_node(target_node);
            }

            true
        } else {
            false
        }
    }

    /// Get the number of elements
    pub fn len(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the height of the skiplist
    pub fn skiplist_height(&self) -> usize {
        self.skiplist.max_height
    }
}

impl<K: Clone + Ord + Hash + Default, V: Clone + Default> Default for SkipTrie<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Drop for SkipTrie<K, V> {
    fn drop(&mut self) {
        // Clean up skiplist nodes
        unsafe {
            let head = self.skiplist.head.load(Ordering::Acquire);
            if !head.is_null() {
                // Collect all nodes by traversing at level 0 (every node has level 0)
                let mut nodes_to_drop = Vec::new();
                let mut curr = (*head).next[0].load(Ordering::Acquire);

                while !curr.is_null() {
                    let next = (*curr).next[0].load(Ordering::Acquire);
                    nodes_to_drop.push(curr);
                    curr = next;
                }

                // Drop all collected nodes
                for node in nodes_to_drop {
                    drop(Box::from_raw(node));
                }

                // Drop the head node
                drop(Box::from_raw(head));
            }
        }
    }
}

unsafe impl<K: Send, V: Send> Send for SkipTrie<K, V> {}
unsafe impl<K: Send + Sync, V: Send + Sync> Sync for SkipTrie<K, V> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_basic_operations() {
        let trie = SkipTrie::new();

        // Test insert
        assert!(trie.insert(5, "five"));
        assert!(trie.insert(3, "three"));
        assert!(trie.insert(7, "seven"));
        assert!(trie.insert(1, "one"));

        // Test find (exact matches)
        assert_eq!(trie.find(&5), Some("five"));
        assert_eq!(trie.find(&3), Some("three"));
        assert_eq!(trie.find(&7), Some("seven"));
        assert_eq!(trie.find(&1), Some("one"));
        assert_eq!(trie.find(&10), None);

        // Test predecessor (≤ target)
        assert_eq!(trie.predecessor(&5), Some((5, "five"))); // Exact match
        assert_eq!(trie.predecessor(&4), Some((3, "three"))); // Strict predecessor
        assert_eq!(trie.predecessor(&6), Some((5, "five"))); // Strict predecessor
        assert_eq!(trie.predecessor(&0), None); // No predecessor

        // Test strict predecessor (< target)
        assert_eq!(trie.strict_predecessor(&5), Some((3, "three")));
        assert_eq!(trie.strict_predecessor(&4), Some((3, "three")));
        assert_eq!(trie.strict_predecessor(&1), None); // No strict predecessor

        // Test length
        assert_eq!(trie.len(), 4);
    }

    #[test]
    fn test_delete() {
        let trie = SkipTrie::new();

        trie.insert(1, "one");
        trie.insert(2, "two");
        trie.insert(3, "three");

        assert!(trie.delete(&2));
        assert_eq!(trie.find(&2), None);
        assert_eq!(trie.len(), 2);

        assert!(!trie.delete(&2)); // Already deleted
    }

    #[test]
    fn test_concurrent_operations() {
        let trie = Arc::new(SkipTrie::new());
        let mut handles = vec![];

        // Spawn multiple threads doing inserts
        for i in 0..8 {
            let trie_clone = Arc::clone(&trie);
            let handle = thread::spawn(move || {
                for j in 0..10000 {
                    let key = i * 10000 + j;
                    trie_clone.insert(key, format!("value_{}", key));
                }
            });
            handles.push(handle);
        }

        // Wait for all inserts to complete
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(trie.len(), 80000);

        // Test some exact finds
        assert_eq!(trie.find(&50), Some("value_50".to_string()));
        assert_eq!(trie.find(&75), Some("value_75".to_string()));
        assert_eq!(trie.find(&0), Some("value_0".to_string()));

        // Test some predecessor queries (should return exact matches when they exist)
        assert_eq!(trie.predecessor(&50).unwrap().0, 50); // Exact match
        assert_eq!(trie.predecessor(&75).unwrap().0, 75); // Exact match

        // Test strict predecessors
        if trie.len() > 1 {
            assert!(trie.strict_predecessor(&50).unwrap().0 < 50);
            assert!(trie.strict_predecessor(&75).unwrap().0 < 75);
        }
    }

    #[test]
    fn test_predecessor_exact_match() {
        let trie = SkipTrie::new();

        // Insert test keys
        assert!(trie.insert(1, "one"));
        assert!(trie.insert(3, "three"));
        assert!(trie.insert(5, "five"));
        assert!(trie.insert(7, "seven"));

        // Test exact matches - predecessor should return the key itself
        let result = trie.predecessor(&5);
        assert_eq!(
            result,
            Some((5, "five")),
            "predecessor(5) should return (5, 'five'), got {:?}",
            result
        );

        assert_eq!(trie.predecessor(&1), Some((1, "one")));
        assert_eq!(trie.predecessor(&3), Some((3, "three")));
        assert_eq!(trie.predecessor(&7), Some((7, "seven")));

        // Test in-between values - should return largest key ≤ target
        assert_eq!(trie.predecessor(&2), Some((1, "one")));
        assert_eq!(trie.predecessor(&4), Some((3, "three")));
        assert_eq!(trie.predecessor(&6), Some((5, "five")));
        assert_eq!(trie.predecessor(&8), Some((7, "seven")));

        // Test edge cases
        assert_eq!(trie.predecessor(&0), None);
    }
}

