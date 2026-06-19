//! Single-threaded and atomic reference-counted smart pointers.
//!
//! Provides `RcPointer<T>` for lightweight single-threaded shared ownership, and
//! `ArcPointer<T>` for thread-safe shared ownership. Both store the reference count
//! inline with the value in a single heap allocation for cache efficiency.

use std::ops::Deref;
use std::sync::atomic::AtomicUsize;

/// A simple reference-counted smart pointer for single-threaded use.
///
/// `RcPointer<T>` provides shared ownership of a value of type `T` allocated on the heap.
/// The reference count and the value are stored together in a single heap allocation
/// for cache efficiency.
///
#[derive(Debug)]
pub struct RcPointer<T> {
    counted_raw_ptr: *mut (usize, T),
}

impl<T> RcPointer<T> {
    pub fn new(value: T) -> Self {
        let counted_raw_ptr = Box::into_raw(Box::new((1, value)));

        RcPointer { counted_raw_ptr }
    }

    pub fn get_raw(&self) -> &T {
        unsafe { &(*self.counted_raw_ptr).1 }
    }

    /// # Safety
    /// Caller must ensure no other references (shared or mutable) to the inner
    /// value exist simultaneously. This is inherently unsound on a ref-counted
    /// pointer with clones — use only when you can guarantee exclusive access.
    pub unsafe fn get_raw_mut(&mut self) -> &mut T {
        unsafe { &mut (*self.counted_raw_ptr).1 }
    }
}

// Implement Deref to allow automatic dereferencing
impl<T> Deref for RcPointer<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.get_raw()
    }
}

// Implement Clone trait.
//
impl<T> Clone for RcPointer<T> {
    fn clone(&self) -> Self {
        // Increment ref counter.
        unsafe {
            (*self.counted_raw_ptr).0 += 1;
        }

        RcPointer {
            counted_raw_ptr: self.counted_raw_ptr,
        }
    }
}

// Implement Drop trait.
//
impl<T> Drop for RcPointer<T> {
    fn drop(&mut self) {
        unsafe {
            // Assume the pointer is valid and points to an initialized (T, usize) pair.
            //
            let (ref_count, _) = &mut *self.counted_raw_ptr;

            // Decrement the reference count.
            //
            *ref_count -= 1;

            // If this is the last reference, deallocate the memory.
            //
            if *ref_count == 0 {
                // The Box destructor will automatically:
                // 1. Call drop() on the tuple
                // 2. Call drop() on T
                // 3. Deallocate the memory
                //
                let _ = Box::from_raw(self.counted_raw_ptr);
            }
        }
    }
}

/// Inline storage for `ArcPointer`: atomic ref-count followed by the value.
/// Using a named struct (instead of a tuple) enables `Unsize` coercions,
/// so `Box<ArcInner<ConcreteType>>` coerces to `Box<ArcInner<dyn Trait>>`.
pub(crate) struct ArcInner<T: ?Sized> {
    pub(crate) ref_count: AtomicUsize,
    pub(crate) value: T,
}

/// A thread-safe reference-counted smart pointer for multi-threaded use.
///
/// `ArcPointer<T>` provides shared ownership of a value of type `T` allocated on the heap
/// across multiple threads. The atomic reference count and the value are stored together
/// in a single heap allocation for cache efficiency.
///
#[derive(Debug)]
pub struct ArcPointer<T: ?Sized> {
    ptr: *mut ArcInner<T>,
}

impl<T> ArcPointer<T> {
    pub fn new(value: T) -> Self {
        let ptr = Box::into_raw(Box::new(ArcInner {
            ref_count: AtomicUsize::new(1),
            value,
        }));

        ArcPointer { ptr }
    }
}

#[macro_export]
macro_rules! arc_pointer_dyn {
    ($value:expr, dyn $($trait:tt)+) => {{
        #[inline(always)]
        fn __arc_dyn_helper<T: $($trait)+ + 'static>(
            value: T,
        ) -> ArcPointer<dyn $($trait)+> {
            use std::sync::atomic::AtomicUsize;
            use $crate::rc_pointer::ArcInner;
            let boxed: Box<ArcInner<dyn $($trait)+>> = Box::new(ArcInner {
                ref_count: AtomicUsize::new(1),
                value,
            });
            ArcPointer::from_raw_inner(Box::into_raw(boxed))
        }
        __arc_dyn_helper($value)
    }};
}

impl<T: ?Sized> ArcPointer<T> {
    /// Constructs an `ArcPointer` from a raw pointer to an `ArcInner`.
    /// The caller must ensure `ptr` was produced by `Box::into_raw`.
    pub(crate) fn from_raw_inner(ptr: *mut ArcInner<T>) -> Self {
        ArcPointer { ptr }
    }

    pub fn get_raw(&self) -> &T {
        unsafe { &(*self.ptr).value }
    }

    /// # Safety
    /// Caller must ensure no other references (shared or mutable) to the inner
    /// value exist simultaneously. This is inherently unsound on a ref-counted
    /// pointer with clones — use only when you can guarantee exclusive access.
    pub unsafe fn get_raw_mut(&mut self) -> &mut T {
        unsafe { &mut (*self.ptr).value }
    }
}

unsafe impl<T: Send + Sync> Send for ArcPointer<T> {}
unsafe impl<T: Send + Sync> Sync for ArcPointer<T> {}

// Implement Deref to allow automatic dereferencing.
//
impl<T: ?Sized> Deref for ArcPointer<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.get_raw()
    }
}

// Implement Clone trait.
//
impl<T: ?Sized> Clone for ArcPointer<T> {
    fn clone(&self) -> Self {
        // Increment ref counter.
        unsafe {
            (*self.ptr)
                .ref_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        ArcPointer { ptr: self.ptr }
    }
}

// Implement Drop trait.
//
impl<T: ?Sized> Drop for ArcPointer<T> {
    fn drop(&mut self) {
        unsafe {
            // Decrement the reference count.
            //
            let ref_count = (*self.ptr)
                .ref_count
                .fetch_sub(1, std::sync::atomic::Ordering::Release);

            // If this is the last reference, deallocate the memory.
            //
            if ref_count == 1 {
                // Ensure all writes from other threads that previously held
                // a reference are visible before we drop the inner value.
                //
                std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                // The Box destructor will automatically:
                // 1. Call drop() on ArcInner
                // 2. Call drop() on T
                // 3. Deallocate the memory
                //
                let _ = Box::from_raw(self.ptr);
            }
        }
    }
}
