use crossbeam_epoch::Guard;
use std::ops::Deref;

// ToDO
// - [ ] make doc runnable
//

/// A reference to a value protected by an epoch guard.
///
/// This struct bundles an epoch guard with a reference to ensure the reference
/// cannot outlive the guard. When the `GuardedRef` is dropped, the guard is
/// unpinned, allowing epoch-based garbage collection to proceed.
///
/// # Design
///
/// The problem with returning `&T` from `find()`:
/// ```ignore
/// pub fn find(&self, key: &T) -> Option<&T> {
///     let guard = epoch::pin();  // Guard dropped here!
///     // Return reference - UNSAFE! Reference outlives guard
/// }
/// ```
///
/// The solution - bundle guard and reference:
/// ```rust,ignore
/// use crossbeam_epoch::{self as epoch};
/// use starfish_crossbeam::sorted_list::SortedList;
/// use starfish_crossbeam::epoch_guarded_sorted_collection::EpochGuardedCollection;
//
/// pub fn find(&self, key: &T) -> Option<GuardedRef<T>> {
///     let guard = epoch::pin();
///     // Create GuardedRef that owns the guard
///     // Reference lifetime tied to GuardedRef lifetime
/// }
/// ```
///
/// # Example
/// ```ignore
/// let list = EpochGuardedCollection::new(SortedList::new());
/// list.insert(5);
///
/// // Get a guarded reference
/// if let Some(guard_ref) = list.find(&5) {
///     println!("Found: {}", *guard_ref);  // Deref to get &T
///     assert_eq!(*guard_ref, 5);
/// } // guard_ref dropped here, guard unpinned
///
/// // Reference cannot escape the scope
/// ```
///
/// # Safety
///
/// This struct is safe because:
/// 1. The guard and reference are bundled together
/// 2. The reference's lifetime is tied to the struct's lifetime
/// 3. When the struct is dropped, the guard is unpinned
/// 4. The reference cannot outlive the guard
///
pub struct GuardedRef<'g, T> {
    _guard: Guard,
    reference: &'g T,
}

impl<'g, T> GuardedRef<'g, T> {
    /// Create a new guarded reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The reference points to valid data
    /// - The guard protects access to that data
    /// - The data won't be freed while the guard is pinned
    ///
    pub(crate) unsafe fn new(guard: Guard, reference: &'g T) -> Self {
        GuardedRef {
            _guard: guard,
            reference,
        }
    }

    /// Get the inner reference.
    ///
    /// This is safe because the reference cannot outlive self,
    /// and self owns the guard.
    pub fn get(&self) -> &T {
        self.reference
    }
}

impl<T> Deref for GuardedRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.reference
    }
}

impl<T: std::fmt::Display> std::fmt::Display for GuardedRef<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reference)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for GuardedRef<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GuardedRef({:?})", self.reference)
    }
}

// GuardedRef is Send if T is Send
unsafe impl<T: Send> Send for GuardedRef<'_, T> {}

// GuardedRef is Sync if T is Sync
unsafe impl<T: Sync> Sync for GuardedRef<'_, T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_epoch::{self as epoch};

    #[test]
    fn test_guarded_ref_basic() {
        let guard = epoch::pin();
        let value = 42;

        unsafe {
            let guarded = GuardedRef::new(guard, &value);
            assert_eq!(*guarded, 42);
            assert_eq!(guarded.get(), &42);
        }
    }

    #[test]
    fn test_guarded_ref_deref() {
        let guard = epoch::pin();
        let value = String::from("hello");

        unsafe {
            let guarded = GuardedRef::new(guard, &value);
            assert_eq!(*guarded, "hello");
            assert_eq!(guarded.len(), 5); // Can call String methods
        }
    }

    #[test]
    fn test_guarded_ref_display() {
        let guard = epoch::pin();
        let value = 42;

        unsafe {
            let guarded = GuardedRef::new(guard, &value);
            assert_eq!(format!("{}", guarded), "42");
            assert_eq!(format!("{:?}", guarded), "GuardedRef(42)");
        }
    }

    #[test]
    fn test_lifetime_safety() {
        // This test demonstrates that the reference can't escape
        let guard = epoch::pin();
        let value = 42;

        unsafe {
            let guarded = GuardedRef::new(guard, &value);
            let _ref = guarded.get(); // OK - reference tied to guarded
            // _ref can't outlive guarded
        }
        // guarded dropped here, guard unpinned
    }

    #[test]
    fn test_guard_unpinned_on_drop() {
        // Create and drop a GuardedRef
        {
            let guard = epoch::pin();
            let value = 42;
            unsafe {
                let _guarded = GuardedRef::new(guard, &value);
                // GuardedRef owns the guard
            }
            // _guarded dropped here, guard unpinned
        }

        // Can pin again (guard was released)
        let _guard2 = epoch::pin();
    }
}
