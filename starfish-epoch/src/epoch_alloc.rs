use std::alloc::{GlobalAlloc, Layout};

use mimalloc::MiMalloc;

pub struct EpochAlloc;

static GLOBAL_MI_ALLOC: MiMalloc = MiMalloc;

#[global_allocator]
static GLOBAL_ALLOC: EpochAlloc = EpochAlloc;

/// Epoch allocator.
///
///
///
unsafe impl GlobalAlloc for EpochAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { GLOBAL_MI_ALLOC.alloc(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { GLOBAL_MI_ALLOC.alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            GLOBAL_MI_ALLOC.dealloc(ptr, layout);
        }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe { GLOBAL_MI_ALLOC.realloc(ptr, layout, new_size) }
    }
}
