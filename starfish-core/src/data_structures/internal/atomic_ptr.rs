//! Atomic pointer utilities for lock-free data structures.
//!
//! Provides `load_consume` (platform-optimized consume ordering) and
//! `prefetch_read` (software prefetch for pointer-chasing traversals).

use std::sync::atomic::{AtomicPtr, Ordering};

// =============================================================================
// load_consume: Relaxed + compiler_fence on ARM, Acquire elsewhere
// =============================================================================
//
// ARM/AArch64 hardware provides data-dependency ordering: if you load a pointer
// and then dereference it, the dereference is guaranteed to see the store that
// created the pointee. This means Relaxed loads are sufficient for pointer-chasing
// traversals (the pattern used in skip list, sorted list, and trie search).
//
// On ARM, Acquire loads emit `ldar` which includes a full load-acquire barrier —
// much more expensive than a plain `ldr`. By using Relaxed + compiler_fence we get
// the same correctness (compiler can't reorder past the fence) with just `ldr`.
//
// On x86/x86_64 (TSO), Acquire loads are plain `mov` anyway, so this is a no-op
// optimization — we just use Acquire directly.
//
// This is the same technique used by crossbeam-utils::AtomicConsume.

/// Load an AtomicPtr with consume ordering semantics.
///
/// On ARM: Relaxed load + compiler fence (leverages hardware data-dependency ordering).
/// On x86: Acquire load (free under TSO).
#[inline(always)]
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
pub(crate) fn load_consume<T>(atom: &AtomicPtr<T>) -> *mut T {
    let result = atom.load(Ordering::Relaxed);
    std::sync::atomic::compiler_fence(Ordering::Acquire);
    result
}

#[inline(always)]
#[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
pub(crate) fn load_consume<T>(atom: &AtomicPtr<T>) -> *mut T {
    atom.load(Ordering::Acquire)
}

// =============================================================================
// prefetch_read: Software prefetch for pointer-chasing data structures
// =============================================================================
//
// In pointer-chasing traversals (skip list, sorted list, trie), the CPU stalls
// on each node load because the next address isn't known until the current load
// completes. By issuing a prefetch for the *next* node's cache line while
// comparing the *current* node's key, we overlap memory latency with useful work.
//
// The prefetch is a hint — the CPU ignores it if the line is already cached or
// the address is invalid (null, unmapped). No correctness impact, only latency.

/// Prefetch a pointer's cache line for reading.
///
/// Issues a platform-specific prefetch hint to bring the cache line containing
/// `ptr` into L1 cache. This is a no-op if the pointer is null or on platforms
/// without prefetch support.
///
/// # Safety
///
/// Safe to call with any pointer value including null and dangling. The prefetch
/// instruction is a hint that never faults.
#[inline(always)]
pub(crate) fn prefetch_read<T>(ptr: *const T) {
    if ptr.is_null() {
        return;
    }

    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: _mm_prefetch is a hint instruction that never faults, even with
        // invalid addresses. _MM_HINT_T0 prefetches into all cache levels (L1/L2/L3).
        unsafe {
            core::arch::x86_64::_mm_prefetch(ptr as *const i8, core::arch::x86_64::_MM_HINT_T0);
        }
    }

    #[cfg(target_arch = "x86")]
    {
        // SAFETY: Same as x86_64 — hint instruction, never faults.
        unsafe {
            core::arch::x86::_mm_prefetch(ptr as *const i8, core::arch::x86::_MM_HINT_T0);
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: PRFM is a hint instruction that never faults, even with
        // invalid addresses. PLDL1KEEP prefetches for reading into L1 cache.
        // We use inline asm because core::arch::aarch64::_prefetch is unstable.
        unsafe {
            core::arch::asm!("prfm pldl1keep, [{ptr}]", ptr = in(reg) ptr, options(nostack, preserves_flags));
        }
    }

    // Other architectures: no-op (prefetch is purely a performance hint)
}
