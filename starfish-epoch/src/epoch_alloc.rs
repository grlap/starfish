//! Global allocator wrapper using MiMalloc.
//!
//! Registers `EpochAlloc` as the `#[global_allocator]`, delegating to
//! MiMalloc for all allocations. Enforces a **minimum allocation of 16
//! bytes** so that epoch-based reclamation can reuse freed object memory
//! for intrusive list nodes (`DeferredAllocation` = 16 bytes).

use std::alloc::{GlobalAlloc, Layout};

use mimalloc::MiMalloc;

/// Minimum allocation size in bytes. Ensures every allocation is large
/// enough to hold a `DeferredAllocation` intrusive node (16 bytes) when
/// the memory is later reused by deferred-delete.
const MIN_ALLOC_SIZE: usize = 16;

pub struct EpochAlloc;

static GLOBAL_MI_ALLOC: MiMalloc = MiMalloc;

#[global_allocator]
static GLOBAL_ALLOC: EpochAlloc = EpochAlloc;

/// Epoch allocator — delegates to MiMalloc with a 16-byte minimum.
///
unsafe impl GlobalAlloc for EpochAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { GLOBAL_MI_ALLOC.alloc(Self::pad(layout)) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { GLOBAL_MI_ALLOC.alloc_zeroed(Self::pad(layout)) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            GLOBAL_MI_ALLOC.dealloc(ptr, Self::pad(layout));
        }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_size = new_size.max(MIN_ALLOC_SIZE);
        unsafe { GLOBAL_MI_ALLOC.realloc(ptr, Self::pad(layout), new_size) }
    }
}

impl EpochAlloc {
    // Pads a layout so size >= MIN_ALLOC_SIZE and align >= align_of::<DeferredAllocation>().
    //
    // This ensures every allocation can later be reused as an intrusive list node
    // by DeferredList::append, which requires both sufficient size (16 bytes) and
    // sufficient alignment (8 bytes for the pointer field).
    //
    #[inline]
    fn pad(layout: Layout) -> Layout {
        use std::mem::align_of;

        // DeferredAllocation requires 8-byte alignment (contains a pointer).
        const MIN_ALLOC_ALIGN: usize = align_of::<crate::epoch::DeferredAllocation>();

        let size = layout.size().max(MIN_ALLOC_SIZE);
        let align = layout.align().max(MIN_ALLOC_ALIGN);

        // SAFETY: `align` is a power of two (max of two powers of two).
        // `size` >= MIN_ALLOC_SIZE (16) >= MIN_ALLOC_ALIGN (8), so size is
        // a multiple of align when align <= 16. For align > 16 (exotic
        // over-aligned types), size is also bumped above, ensuring size >= align.
        // Layout requires size to be a multiple of align — round up if needed.
        let size = (size + align - 1) & !(align - 1);
        unsafe { Layout::from_size_align_unchecked(size, align) }
    }
}
