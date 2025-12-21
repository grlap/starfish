use std::ops::{Deref, DerefMut};
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

    pub fn get_raw_mut(&mut self) -> &mut T {
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

// Implement DerefMut trait for mutable dereferencing.
//
impl<T> DerefMut for RcPointer<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.get_raw_mut()
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

/// A thread-safe reference-counted smart pointer for multi-threaded use.
///
/// `ArcPointer<T>` provides shared ownership of a value of type `T` allocated on the heap
/// across multiple threads. The atomic reference count and the value are stored together
/// in a single heap allocation for cache efficiency.
///
#[derive(Debug)]
pub struct ArcPointer<T: ?Sized> {
    pub(super) counted_raw_ptr: *mut (AtomicUsize, T),
}

impl<T> ArcPointer<T> {
    pub fn new(value: T) -> Self {
        let counted_raw_ptr = Box::into_raw(Box::new((AtomicUsize::new(1), value)));

        ArcPointer { counted_raw_ptr }
    }
}

#[macro_export]
macro_rules! arc_pointer_dyn {
    ($value:expr, dyn $($trait:tt)+) => {{
        #[inline(always)]
        fn __arc_dyn_helper<T>(value: T) -> ArcPointer<dyn $($trait)+>
        where
            T: $($trait)+ + 'static,
        {
            use std::sync::atomic::AtomicUsize;

            let counted = Box::new((AtomicUsize::new(1), value));
            let raw = Box::into_raw(counted);

            unsafe {
                let trait_ref: &(dyn $($trait)+) = &(*raw).1;
                let trait_ptr_parts: [usize; 2] = std::mem::transmute_copy(&trait_ref);
                let vtable = trait_ptr_parts[1];
                let fat_ptr_parts: [usize; 2] = [raw as usize, vtable];
                let fat_ptr: *mut (AtomicUsize, dyn $($trait)+) = std::mem::transmute(fat_ptr_parts);

                ArcPointer { counted_raw_ptr: fat_ptr }
            }
        }

        __arc_dyn_helper($value)
    }};
}

impl<T: ?Sized> ArcPointer<T> {
    pub fn get_raw(&self) -> &T {
        unsafe { &(*self.counted_raw_ptr).1 }
    }

    pub fn get_raw_mut(&mut self) -> &mut T {
        unsafe { &mut (*self.counted_raw_ptr).1 }
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

// Implement DerefMut trait for mutable dereferencing.
//
impl<T: ?Sized> DerefMut for ArcPointer<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.get_raw_mut()
    }
}

// Implement Clone trait.
//
impl<T: ?Sized> Clone for ArcPointer<T> {
    fn clone(&self) -> Self {
        // Increment ref counter.
        unsafe {
            (*self.counted_raw_ptr)
                .0
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        ArcPointer {
            counted_raw_ptr: self.counted_raw_ptr,
        }
    }
}

// Implement Drop trait.
//
impl<T: ?Sized> Drop for ArcPointer<T> {
    fn drop(&mut self) {
        unsafe {
            // Assume the pointer is valid and points to an initialized (T, usize) pair.
            //
            // Decrement the reference count.
            //
            let ref_count = (*self.counted_raw_ptr)
                .0
                .fetch_sub(1, std::sync::atomic::Ordering::Release);

            // If this is the last reference, deallocate the memory.
            //
            if ref_count == 1 {
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
