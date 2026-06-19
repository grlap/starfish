# Iterators and Range Queries for Starfish SkipList

## Status: PARTIAL

This document outlines the design for adding iterator and range query support to Starfish SkipList.

## Why This Matters

**API Parity with Crossbeam:**
- Crossbeam skiplist provides `iter()`, `range()`, `lower_bound()`, `upper_bound()`
- Essential for production use cases
- Enables foreach patterns and range scans

**Performance Opportunity:**
- Starfish's lock-free design should enable faster iteration than Crossbeam
- No validation overhead during traversal
- Direct level-0 traversal (all nodes linked at bottom level)

---

## Current State

**Traits Defined:** ✅
```rust
// starfish-core/src/data_structures/iterable_collection.rs
pub trait IterableCollection<T> {
    type GuardedRef<'a>: Deref<Target = T>;
    type Iter<'a>: Iterator<Item = Self::GuardedRef<'a>>;

    fn iter(&self) -> Self::Iter<'_>;
    fn len(&self) -> usize;
}

pub trait IterableSortedCollection<T: Ord>: IterableCollection<T> {
    fn iter_from(&self, start_key: &T) -> Self::Iter<'_>;
}
```

**Implementation (via `SortedCollection` default methods):**
| Method | Status | Notes |
|--------|--------|-------|
| `iter()` | ✅ | Returns `SortedCollectionIter` over level-0 nodes |
| `range(R)` | ✅ | Returns `SortedCollectionRange`, accepts any `RangeBounds<T>` |
| `iter_from()` | ❌ | Not yet implemented |
| `lower_bound()` | ❌ | Not yet implemented |
| `upper_bound()` | ❌ | Not yet implemented |

`iter()` and `range()` are provided as default methods on `SortedCollection` and work
for all implementors (SkipList, SortedList, SkipTrie). They return zero-copy
`CollectionRef<'a, T>` references.

---

## Crossbeam API Reference

From [crossbeam-skiplist docs](https://docs.rs/crossbeam-skiplist/latest/crossbeam_skiplist/struct.SkipSet.html):

```rust
// Basic iteration
fn iter(&self) -> Iter<'_, T>

// Range queries
fn range<Q, R>(&self, range: R) -> Range<'_, Q, R, T>
    where R: RangeBounds<Q>

// Bound queries
fn lower_bound<'a, Q>(&'a self, bound: Bound<&Q>) -> Option<Entry<'a, T>>
fn upper_bound<'a, Q>(&'a self, bound: Bound<&Q>) -> Option<Entry<'a, T>>
```

**Usage examples:**
```rust
// Iterate all elements
for entry in skipset.iter() {
    println!("{}", entry.value());
}

// Range query
for entry in skipset.range(5..=10) {
    println!("{}", entry.value());
}

// Find first element >= 5
if let Some(entry) = skipset.lower_bound(Included(&5)) {
    println!("Found: {}", entry.value());
}
```

---

## Design: Priority 1 - Basic Iterator

### Iterator Struct

```rust
pub struct SkipListIter<'a, T, G: Guard> {
    /// Current node pointer (level 0)
    current: SkipNodePtr<T>,
    /// Epoch guard to keep nodes alive during iteration
    guard: G::EpochRef<'a>,
    /// Reference to the skip list (for accessing nodes)
    list: &'a SkipList<T, G>,
}
```

### Iterator Implementation

```rust
impl<'a, T: Ord, G: Guard> Iterator for SkipListIter<'a, T, G> {
    type Item = SkipListRef<'a, T, G>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.is_null() {
                return None;
            }

            unsafe {
                let node = &*self.current;

                // Skip sentinel nodes
                if node.is_sentinel() {
                    self.current = node.get_next(0); // Level 0 has all nodes
                    continue;
                }

                // Get next pointer before returning current
                let next = node.get_next(0);

                // Check if marked for deletion
                if MarkedPtr::new(next).is_any_marked() {
                    // Skip logically deleted nodes
                    self.current = MarkedPtr::unmask(next);
                    continue;
                }

                // Create guarded reference
                let item = SkipListRef {
                    value: node.key(),
                    _guard: &self.guard,
                };

                // Advance iterator
                self.current = next;

                return Some(item);
            }
        }
    }
}
```

### Guarded Reference

```rust
pub struct SkipListRef<'a, T, G: Guard> {
    value: &'a T,
    _guard: &'a G::EpochRef<'a>,
}

impl<'a, T, G: Guard> Deref for SkipListRef<'a, T, G> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value
    }
}
```

### SkipList Implementation

```rust
impl<T: Ord, G: Guard> SkipList<T, G> {
    pub fn iter(&self) -> SkipListIter<'_, T, G> {
        let guard = G::pin();
        let head = self.head();

        // Start from first non-sentinel node
        let first = unsafe {
            (*head).get_next(0) // Level 0 traversal
        };

        SkipListIter {
            current: first,
            guard,
            list: self,
        }
    }
}
```

---

## Design: Priority 2 - Range Queries

### Range Iterator

```rust
pub struct SkipListRange<'a, T, G: Guard, R: RangeBounds<T>> {
    iter: SkipListIter<'a, T, G>,
    range: R,
    done: bool,
}

impl<'a, T: Ord, G: Guard, R: RangeBounds<T>> Iterator for SkipListRange<'a, T, G, R> {
    type Item = SkipListRef<'a, T, G>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            let item = self.iter.next()?;

            // Check if within range
            match self.range.end_bound() {
                Bound::Included(end) => {
                    if item.deref() > end {
                        self.done = true;
                        return None;
                    }
                }
                Bound::Excluded(end) => {
                    if item.deref() >= end {
                        self.done = true;
                        return None;
                    }
                }
                Bound::Unbounded => {}
            }

            // Check start bound
            match self.range.start_bound() {
                Bound::Included(start) => {
                    if item.deref() >= start {
                        return Some(item);
                    }
                }
                Bound::Excluded(start) => {
                    if item.deref() > start {
                        return Some(item);
                    }
                }
                Bound::Unbounded => return Some(item),
            }
        }
    }
}
```

### SkipList Range Method

```rust
impl<T: Ord, G: Guard> SkipList<T, G> {
    pub fn range<R: RangeBounds<T>>(&self, range: R) -> SkipListRange<'_, T, G, R> {
        let guard = G::pin();

        // Find start position using search_key
        let start_node = match range.start_bound() {
            Bound::Included(key) | Bound::Excluded(key) => {
                // Use existing search_key to find position
                self.search_key_with_guard(key, &guard)
            }
            Bound::Unbounded => unsafe { (*self.head()).get_next(0) },
        };

        SkipListRange {
            iter: SkipListIter {
                current: start_node,
                guard,
                list: self,
            },
            range,
            done: false,
        }
    }
}
```

---

## Design: Priority 3 - Bound Queries

```rust
impl<T: Ord, G: Guard> SkipList<T, G> {
    /// Find first element >= bound
    pub fn lower_bound(&self, bound: Bound<&T>) -> Option<SkipListRef<'_, T, G>> {
        let guard = G::pin();

        match bound {
            Bound::Included(key) => {
                // Find key or next greater
                let node = self.search_key_or_next(key, &guard)?;
                Some(SkipListRef {
                    value: unsafe { (*node).key() },
                    _guard: &guard,
                })
            }
            Bound::Excluded(key) => {
                // Find next greater
                let node = self.search_next_greater(key, &guard)?;
                Some(SkipListRef {
                    value: unsafe { (*node).key() },
                    _guard: &guard,
                })
            }
            Bound::Unbounded => {
                // Return first element
                self.iter().next()
            }
        }
    }

    /// Find last element <= bound
    pub fn upper_bound(&self, bound: Bound<&T>) -> Option<SkipListRef<'_, T, G>> {
        // Similar implementation, traverse backwards or use predecessors
        todo!()
    }
}
```

---

## Technical Challenges

### 1. Guard Lifetime Management

**Problem:** Iterator needs to keep guard alive for entire iteration
**Solution:** Store guard in iterator struct, tie lifetime to iterator

### 2. Marked Node Handling

**Problem:** Nodes may be marked for deletion during iteration
**Solution:** Skip marked nodes in iterator loop (acceptable to miss concurrent deletions)

### 3. Memory Ordering

**Problem:** Need correct synchronization with concurrent modifications
**Solution:** Use `load_consume` (existing pattern) for level-0 traversal

### 4. Sentinel Nodes

**Problem:** Head/tail sentinels should not be visible to users
**Solution:** Check `is_sentinel()` and skip in iterator

### 5. Search with External Guard

**Problem:** Current `search_key` creates guard internally
**Solution:** Add `search_key_with_guard` variant that accepts external guard

---

## Implementation Order

1. **Phase 1: Basic Iterator** (2-3 days)
   - Implement `SkipListIter` struct
   - Implement `Iterator` trait
   - Implement `iter()` method
   - Add tests

2. **Phase 2: Range Queries** (1-2 days)
   - Implement `SkipListRange` struct
   - Implement `range()` method
   - Add bound checking logic
   - Add tests

3. **Phase 3: Bound Queries** (1-2 days)
   - Implement `lower_bound()` and `upper_bound()`
   - Add helper methods for "or_next" searches
   - Add tests

4. **Phase 4: Benchmarks** (1 day)
   - Compare iteration performance vs Crossbeam
   - Benchmark range queries
   - Add to benchmarks.md

---

## Expected Performance

**Hypothesis:** Starfish iteration should be **faster** than Crossbeam because:
1. No validation overhead (Crossbeam validates metadata on each access)
2. Direct pointer chasing at level 0
3. `load_consume` on ARM (cheaper than Acquire)
4. No epoch barriers during traversal

**Benchmark targets:**
- Iterate 500K elements: < 5ms (vs Crossbeam baseline)
- Range query 10K elements: < 1ms

---

## API Example

```rust
use starfish_core::SkipList;
use starfish_crossbeam::EpochGuard;

let list: SkipList<i32, EpochGuard> = SkipList::new();

// Insert elements
for i in 0..1000 {
    list.insert(i);
}

// Iterate all
for val in list.iter() {
    println!("{}", *val);
}

// Range query
for val in list.range(100..200) {
    println!("{}", *val);
}

// Find bounds
if let Some(val) = list.lower_bound(Included(&50)) {
    println!("First >= 50: {}", *val);
}
```

---

## References

- [Crossbeam SkipSet docs](https://docs.rs/crossbeam-skiplist/latest/crossbeam_skiplist/struct.SkipSet.html)
- [Crossbeam Range iterator](https://tikv.github.io/doc/crossbeam_skiplist/map/struct.Range.html)
- Starfish benchmarks: [benchmarks.md](../../starfish-crossbeam/docs/benchmarks.md)

---

## Next Steps

1. Implement basic `iter()` - highest priority
2. Add benchmarks comparing to Crossbeam
3. Extend to range queries once iteration is solid
4. Consider async iterator support later
