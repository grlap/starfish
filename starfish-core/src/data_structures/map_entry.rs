//! `MapEntry<K, V>`: a map payload whose value lives behind an atomic pointer.
//!
//! Unlike [`Pair<K, V>`](super::pair::Pair) (value stored inline), `MapEntry` keeps
//! `key: K` inline and immutable and the value behind an `AtomicPtr<V>`. This is what
//! makes a lock-free `update` **epoch-safe**: CAS the value pointer and retire the OLD
//! value through the epoch reclaimer. The node is never replaced, so there is no
//! old-node-reachability hazard — the failure mode of the removed SkipList *helping*
//! node-replacement UPDATE under real reclamation (a node `defer_destroy`'d while a
//! traversal could still reach it; the non-helping node-replacement in
//! SortedList/SkipTrie is validated epoch-safe). Ordering/equality are by `key` only,
//! so a `SortedCollection` keeps the entry in place while its value mutates.
//!
//! The value pointer doubles as the entry's **logical presence** state:
//! - non-null  → entry present, pointer is the current value;
//! - null      → **tombstone**: the entry was claimed by `remove` and is logically
//!   absent, even while its node is still physically linked (remove claims FIRST,
//!   then marks + unlinks the node). All readers must treat null as "no entry".
//!
//! This single atomic carries the update↔remove linearization: `remove` linearizes
//! at [`claim_value`](MapEntry::claim_value) (unique winner), `update` linearizes at
//! [`replace_value`](MapEntry::replace_value) (CAS that fails on the tombstone), so
//! an update can never succeed after the remove it races — the remover always
//! returns the then-current value.
//!
//! Value-lifetime invariants (each `V` freed exactly once, always through one path):
//! - `value` holds a `Box::into_raw(V)` pointer; non-null for a live entry.
//! - The CURRENT value is freed by [`Drop`] when the node is reclaimed (null-checked:
//!   a claimed entry's value was already taken by the remover).
//! - A REPLACED value is retired by the updater, and a CLAIMED value by the remover,
//!   via the guard's `defer_destroy` (using [`MapEntry::drop_value`]) — never freed
//!   inline, because a pinned reader may still hold a borrow of it.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::sync::atomic::{AtomicPtr, Ordering as AtomicOrdering};

/// A key-value entry that orders/compares by `key` only, with the value held behind
/// an atomic pointer so it can be replaced in place (see module docs).
pub struct MapEntry<K, V> {
    key: K,
    value: AtomicPtr<V>,
}

// `AtomicPtr<V>` is unconditionally Send + Sync, so the auto-derived impls would
// ignore `V` entirely — even though the entry semantically OWNS a `Box<V>` and hands
// out `&V` across threads. Without these explicit bounds,
// `SkipList<MapEntry<K, Rc<..>>>` would be Sync and two threads could clone the same
// `Rc` concurrently — a data race from 100% safe code.
//
// SAFETY: `MapEntry` owns its value like `Box<V>` — sending the entry to another
// thread moves the boxed `V` (drop/claim may then happen there), so `V: Send` is
// required exactly as for `Box<V>`.
unsafe impl<K: Send, V: Send> Send for MapEntry<K, V> {}
// SAFETY: through `&MapEntry`, threads can (a) borrow the value (deref of
// `value_ptr`), which requires `V: Sync`, and (b) replace/claim the value
// (`replace_value`/`claim_value`), which hands the owned `V` to another thread and
// requires `V: Send`.
unsafe impl<K: Sync, V: Send + Sync> Sync for MapEntry<K, V> {}

impl<K, V> MapEntry<K, V> {
    /// Create an entry owning a freshly-boxed `value`.
    #[inline]
    pub fn new(key: K, value: V) -> Self {
        Self {
            key,
            value: AtomicPtr::new(Box::into_raw(Box::new(value))),
        }
    }

    /// The immutable key.
    #[inline]
    pub fn key(&self) -> &K {
        &self.key
    }

    /// The current value pointer (Acquire load). **Null means the entry is a
    /// tombstone (logically removed)** — callers must check before dereferencing.
    /// Hand a non-null pointer to [`Guard::make_ref`](crate::guard::Guard::make_ref)
    /// to build a guard-protected value handle; the caller must hold a pinned guard
    /// across the load + `make_ref`.
    #[inline]
    pub(crate) fn value_ptr(&self) -> *const V {
        self.value.load(AtomicOrdering::Acquire)
    }

    /// Atomically install `new` as the value, returning the previous pointer for the
    /// caller to retire via the epoch guard — or `None` (with `new` dropped) if the
    /// entry was already claimed by a remover (null tombstone).
    ///
    /// This is the map-UPDATE linearization point. Failing on the tombstone is what
    /// makes `update` linearizable against `remove`: an update can only succeed
    /// strictly before the remove's claim, so a remover always returns the
    /// then-current value (including a just-installed update).
    pub(crate) fn replace_value(&self, new: V) -> Option<*mut V> {
        let new_ptr = Box::into_raw(Box::new(new));
        let mut old = self.value.load(AtomicOrdering::Acquire);
        loop {
            if old.is_null() {
                // Entry logically removed (claimed): the update linearizes after the
                // remove and fails. Reclaim the never-published new value.
                // SAFETY: `new_ptr` came from `Box::into_raw` above and was never
                // published — we have exclusive ownership.
                unsafe { drop(Box::from_raw(new_ptr)) };
                return None;
            }
            match self.value.compare_exchange_weak(
                old,
                new_ptr,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            ) {
                Ok(_) => return Some(old),
                Err(actual) => old = actual,
            }
        }
    }

    /// Atomically claim the value, leaving a null **tombstone**: the map-REMOVE
    /// linearization point. Exactly one claimer wins per entry lifetime; the winner
    /// receives the value pointer to clone-then-retire via the epoch guard (a pinned
    /// reader may still hold a borrow of it — never free it inline). Returns `None`
    /// if the entry was already claimed.
    ///
    /// After a claim, the entry reads as ABSENT (`value_ptr()` is null) even though
    /// its node may still be physically linked and unmarked for a short window —
    /// readers must treat a null value pointer as "no entry".
    #[inline]
    pub(crate) fn claim_value(&self) -> Option<*mut V> {
        let old = self
            .value
            .swap(std::ptr::null_mut(), AtomicOrdering::AcqRel);
        if old.is_null() { None } else { Some(old) }
    }

    /// `dealloc` function handed to `Guard::defer_destroy` for a retired value.
    ///
    /// # Safety
    /// `p` must be a `Box::into_raw(V)` pointer no longer reachable by any thread.
    pub(crate) unsafe fn drop_value(p: *mut V) {
        // SAFETY: caller's contract — `p` came from `Box::into_raw` and is unreachable.
        unsafe { drop(Box::from_raw(p)) };
    }
}

impl<K, V> Drop for MapEntry<K, V> {
    fn drop(&mut self) {
        // Frees the CURRENT value when the node is reclaimed. Replaced values were
        // already retired via `defer_destroy`, so they are not seen here.
        let p = *self.value.get_mut();
        if !p.is_null() {
            // SAFETY: `value` always holds a `Box::into_raw` pointer (or null).
            unsafe { drop(Box::from_raw(p)) };
        }
    }
}

// Lookups go through `T: Borrow<K>`, so the collection can be searched by `&K`.
impl<K, V> Borrow<K> for MapEntry<K, V> {
    #[inline]
    fn borrow(&self) -> &K {
        &self.key
    }
}

// Ordering / equality by KEY only (the value is mutable and irrelevant to position).
impl<K: PartialEq, V> PartialEq for MapEntry<K, V> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl<K: Eq, V> Eq for MapEntry<K, V> {}
impl<K: PartialOrd, V> PartialOrd for MapEntry<K, V> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.key.partial_cmp(&other.key)
    }
}
impl<K: Ord, V> Ord for MapEntry<K, V> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

#[cfg(test)]
mod tests {
    use super::MapEntry;

    // Send/Sync regression guard: `MapEntry` owns its value like `Box<V>`; a non-Send/
    // non-Sync `V` (e.g. `Rc`) must make the entry — and any collection of entries —
    // non-Send/non-Sync, despite `AtomicPtr<V>` being unconditionally Send + Sync.
    static_assertions::assert_not_impl_any!(MapEntry<i32, std::rc::Rc<String>>: Send, Sync);
    static_assertions::assert_not_impl_any!(MapEntry<i32, std::cell::Cell<u32>>: Sync);
    static_assertions::assert_impl_all!(MapEntry<i32, String>: Send, Sync);
    static_assertions::assert_impl_all!(
        crate::data_structures::SkipList<MapEntry<i32, String>, crate::DeferredGuard>: Send, Sync
    );
    static_assertions::assert_not_impl_any!(
        crate::data_structures::SkipList<MapEntry<i32, std::rc::Rc<String>>, crate::DeferredGuard>: Send, Sync
    );
}
