//! Deferred guard implementation for testing.
//!
//! This module provides `DeferredGuard`, a simple guard implementation that
//! defers all node destruction until the guard is dropped.

use std::collections::HashSet;
use std::ops::Deref;
use std::sync::Mutex;

use super::Guard;

/// A simple guard that defers all node destruction until the guard is dropped.
///
/// This is useful for testing where you want predictable destruction timing.
/// Not suitable for production use in long-running applications as memory
/// will accumulate until the guard is dropped.
///
/// # Thread Safety
///
/// `DeferredGuard` uses a `Mutex` internally to safely collect nodes from
/// multiple threads. The nodes are freed when the guard is dropped.
///
pub struct DeferredGuard {
    deferred: Mutex<Vec<DeferredNode>>,
    #[cfg(debug_assertions)]
    seen: Mutex<HashSet<usize>>,
}

struct DeferredNode {
    ptr: *mut (),
    dealloc: unsafe fn(*mut ()),
}

// Safety: DeferredNode is Send because we only store the pointer
// and deallocation function, and ensure proper synchronization via Mutex
unsafe impl Send for DeferredNode {}

impl DeferredGuard {
    /// Create a new deferred guard.
    pub fn new() -> Self {
        DeferredGuard {
            deferred: Mutex::new(Vec::new()),
            #[cfg(debug_assertions)]
            seen: Mutex::new(HashSet::new()),
        }
    }
}

impl Default for DeferredGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DeferredGuard {
    fn drop(&mut self) {
        let nodes = self.deferred.get_mut().unwrap();

        // Check for duplicates before freeing
        let mut seen: HashSet<usize> = HashSet::new();
        let mut dup_count = 0;
        for (i, node) in nodes.iter().enumerate() {
            let addr = node.ptr as usize;
            if !seen.insert(addr) {
                dup_count += 1;
                if dup_count <= 5 {
                    // Find first occurrence
                    let first_idx = nodes.iter().position(|n| n.ptr as usize == addr).unwrap();
                    eprintln!(
                        "DUPLICATE #{}: ptr={:#x} first_at_idx={} dup_at_idx={}",
                        dup_count, addr, first_idx, i
                    );
                }
            }
        }
        if dup_count > 0 {
            eprintln!(
                "Total duplicates: {}, total nodes: {}",
                dup_count,
                nodes.len()
            );
            panic!("Found {} duplicate pointer(s) in deferred list", dup_count);
        }

        for node in nodes.drain(..) {
            // SAFETY: Each pointer was registered exactly once via `defer_destroy`, guaranteed
            // by the runtime duplicate check above (which panics before reaching this loop).
            // The `dealloc` function matches the original type via the transmute at registration.
            unsafe {
                (node.dealloc)(node.ptr);
            }
        }
    }
}

/// A simple reference wrapper for DeferredGuard.
///
/// Since DeferredGuard defers all destruction until drop, references
/// are always valid while the guard exists.
///
pub struct DeferredRef<'a, T> {
    data: &'a T,
}

impl<'a, T> DeferredRef<'a, T> {
    /// Create a new deferred reference.
    pub fn new(data: &'a T) -> Self {
        DeferredRef { data }
    }
}

impl<T> Deref for DeferredRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl Guard for DeferredGuard {
    type GuardedRef<'a, T: 'a> = DeferredRef<'a, T>;

    /// For DeferredGuard, ReadGuard is a no-op since all nodes are protected
    /// until the collection's stored guard drops.
    type ReadGuard = ();

    fn pin() -> Self::ReadGuard {
        // No-op for DeferredGuard - protection is provided by the stored guard
    }

    fn repin(_: &mut Self::ReadGuard) {
        // No-op: deferred reclamation has no epoch state to refresh.
    }

    unsafe fn defer_destroy<N>(&self, node: *mut N, dealloc: unsafe fn(*mut N)) {
        #[cfg(debug_assertions)]
        {
            let addr = node as usize;
            let mut seen = self.seen.lock().unwrap();
            if !seen.insert(addr) {
                panic!("DUPLICATE defer_destroy at {:#x}", addr);
            }
        }

        let node = DeferredNode {
            ptr: node as *mut (),
            // SAFETY: `*mut N` and `*mut ()` have the same size and ABI (both are thin
            // pointers), so transmuting `fn(*mut N)` to `fn(*mut ())` is valid. The erased
            // pointer will only be passed back to this function, which was created for type N.
            dealloc: unsafe {
                std::mem::transmute::<unsafe fn(*mut N), unsafe fn(*mut ())>(dealloc)
            },
        };
        self.deferred.lock().unwrap().push(node);
    }

    unsafe fn make_ref<'a, T: 'a>(ptr: *const T) -> Self::GuardedRef<'a, T> {
        // SAFETY: Caller guarantees ptr is valid for lifetime 'a.
        DeferredRef::new(unsafe { &*ptr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deferred_guard_basic() {
        let guard = DeferredGuard::default();

        // Create some test data
        let boxed = Box::new(42i32);
        let ptr = Box::into_raw(boxed);

        // Defer destruction
        // SAFETY: `ptr` was obtained from `Box::into_raw` above and has not been freed,
        // and the dealloc closure reconstructs the original `Box` to free it.
        unsafe {
            guard.defer_destroy(ptr, |p| {
                drop(Box::from_raw(p));
            });
        }

        // Guard dropped here, node should be freed
    }

    #[test]
    fn test_deferred_ref() {
        let value = 42;
        DeferredGuard::pin(); // ReadGuard is ()

        // SAFETY: `&value` is a valid reference on the stack and remains live for the
        // duration of this block, satisfying `make_ref`'s lifetime requirement.
        unsafe {
            let guarded = DeferredGuard::make_ref(&value);
            assert_eq!(*guarded, 42);
        }
    }

    #[test]
    fn test_multiple_deferred_nodes() {
        let guard = DeferredGuard::default();

        for i in 0..10 {
            let boxed = Box::new(i);
            let ptr = Box::into_raw(boxed);
            // SAFETY: `ptr` was obtained from `Box::into_raw` on the line above and is
            // unique (each loop iteration creates a new allocation).
            unsafe {
                guard.defer_destroy(ptr, |p| {
                    drop(Box::from_raw(p));
                });
            }
        }
        // All 10 nodes freed when guard drops
    }

    #[test]
    fn test_repin_is_noop() {
        DeferredGuard::repin(&mut ());
    }
}
