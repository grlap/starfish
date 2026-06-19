//! Core epoch-based reclamation implementation.
//!
//! Uses a **3-state epoch model**: garbage tagged at epoch E is safe to
//! reclaim once the global epoch reaches E+2, because advancing from E+1
//! to E+2 requires all reactors to have observed E+1 (meaning no reactor
//! can still hold references from epoch E).
//!
//! Key types:
//! - [`Epoch`] — global epoch tracker (shared `AtomicU32` + per-reactor counter pointers)
//! - [`EpochCounter`] — per-reactor state (`local_epoch`, `pin_count`, 3 garbage bags)
//! - [`EpochGuard`] — RAII pin guard that decrements `pin_count` on drop
//!
//! Deferred allocations are stored in intrusive linked lists that reuse the
//! freed object's own memory for bookkeeping (zero extra allocations).
//! `deferred_delete` defers only memory deallocation — the caller must drop
//! the value before scheduling.

use std::alloc::{Layout, dealloc};
use std::cell::UnsafeCell;
use std::cmp::min;
use std::marker::PhantomData;
use std::ptr::{self, null_mut};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use starfish_reactor::coordinator::{CooperativeInitializer, Coordinator};
use starfish_reactor::reactor::Reactor;

pub struct Epoch {
    current_epoch_index: AtomicU32,
    counters: OnceLock<Vec<AtomicPtr<EpochCounter>>>,
}

// SAFETY: Epoch is shared across reactor threads. Interior mutability is
// provided by AtomicU32 (current_epoch_index) and AtomicPtr (counters slots).
// The OnceLock<Vec<...>> is written once during init() before any reactor
// starts, and only read afterwards. Individual AtomicPtr slots are written
// by their owning reactor (init_reactor/uninit_reactor) and read cross-thread
// by min_local_epoch(), both using proper Acquire/Release ordering.
unsafe impl Send for Epoch {}
unsafe impl Sync for Epoch {}

impl Epoch {
    pub fn new() -> Self {
        Epoch {
            current_epoch_index: AtomicU32::new(1),
            counters: OnceLock::new(),
        }
    }

    // Pins the current epoch (epoch in use).
    //
    pub fn pin() -> EpochGuard {
        let epoch_counter = EpochCounter::local_instance();

        epoch_counter.pin()
    }

    // Returns the local epoch observed by the current reactor.
    //
    pub fn local_epoch() -> u32 {
        EpochCounter::local_instance().local_epoch()
    }

    // Attempts to advance the global epoch by one.
    //
    // Succeeds only when all reactors have observed the current epoch
    // (i.e., min(local_epoch) >= current_epoch_index).
    //
    pub fn bump_epoch() -> bool {
        let epoch = Epoch::instance();

        let global = epoch.current_epoch_index.load(Ordering::Acquire);
        let min_local = Self::min_local_epoch();

        if min_local < global {
            // Some reactors haven't caught up to the current epoch yet.
            //
            return false;
        }

        // All reactors are on the current epoch. Safe to advance.
        //
        epoch
            .current_epoch_index
            .compare_exchange(
                global,
                global.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    // Returns the minimum local_epoch across all live reactors.
    //
    // Skips reactors whose counter has been nulled during shutdown.
    // Returns u32::MAX if no live reactors remain.
    //
    pub(crate) fn min_local_epoch() -> u32 {
        let coordinator = Coordinator::instance();
        let counters = Epoch::instance().counters.get().unwrap();
        let reactor_count = coordinator.reactor_count();

        let mut epoch_index = u32::MAX;

        for counter_slot in counters.iter().take(reactor_count) {
            let ptr = counter_slot.load(Ordering::Acquire);
            if ptr.is_null() {
                continue;
            }
            // SAFETY: Non-null pointer was set during init_reactor and remains
            // valid until uninit_reactor stores null (with Release ordering).
            // The Acquire load above synchronizes with that Release store.
            let local = unsafe { &*ptr }.local_epoch.load(Ordering::Acquire);
            epoch_index = min(epoch_index, local);
        }

        epoch_index
    }

    pub fn instance<'a>() -> &'a Self {
        // SAFETY: Thread-local pointer is set by `set_local_instance` during
        // `init_reactor` and lives for the reactor thread's lifetime.
        // Returns &Self (not &mut) because all fields use interior mutability
        // (AtomicU32, OnceLock<Vec<AtomicPtr>>).
        let epoch_ptr = EPOCH_INSTANCE.with(|s| unsafe { *s.get() });

        unsafe { &*epoch_ptr }
    }

    pub(super) fn set_local_instance(&self) {
        EPOCH_INSTANCE.with(|s| {
            // SAFETY: Single-threaded access to the thread-local cell.
            unsafe {
                *s.get() = self;
            };
        });
    }

    pub(super) fn reset_local_instance() {
        EPOCH_INSTANCE.with(|s| {
            // SAFETY: Single-threaded access to the thread-local cell.
            unsafe {
                *s.get() = ptr::null_mut();
            };
        });
    }

    /// Schedules a deferred deallocation for a previously-dropped object.
    ///
    /// This function only defers the memory deallocation (same semantics as
    /// crossbeam's `defer_unchecked` + `dealloc`).
    ///
    /// # Safety
    ///
    /// - `object` must be a non-null pointer to a valid heap allocation
    ///   (allocated via the global allocator, i.e., `EpochAlloc`).
    /// - The value must have already been dropped (`drop_in_place` or equivalent).
    /// - `T` must not be a zero-sized type (ZSTs are not heap-allocated by `Box`).
    /// - The caller must hold an active epoch pin (`Epoch::pin()`).
    ///
    pub unsafe fn deferred_delete<T>(object: *mut T) {
        let layout = Layout::new::<T>();
        debug_assert!(
            layout.size() > 0,
            "deferred_delete: ZSTs have no allocation to defer"
        );
        debug_assert!(
            layout.size() <= u32::MAX as usize && layout.align() <= u32::MAX as usize,
            "deferred_delete: type too large for u32 layout (size={}, align={})",
            layout.size(),
            layout.align(),
        );
        let counter = EpochCounter::local_instance();
        debug_assert!(
            counter.pin_count > 0,
            "deferred_delete called without an active epoch pin"
        );
        let epoch = counter.local_epoch.load(Ordering::Relaxed);
        counter.generations.defer(epoch, object as *mut u8, layout);
    }
}

impl CooperativeInitializer for Epoch {
    // Initialize Epoch Framework.
    //
    fn init(&self) {
        let coordinator = Coordinator::instance();

        // Create EpochCounter pointer slots (one per reactor).
        //
        let _ = self.counters.set(
            (0..coordinator.reactor_count())
                .map(|_| AtomicPtr::new(null_mut()))
                .collect(),
        );
    }

    fn uninit(&self) {
        // No-op: per-reactor cleanup is done in uninit_reactor.
        // The Coordinator does not currently call this trait method;
        // only Epoch::drop() calls the inherent uninit() below.
    }

    fn init_reactor(&self, _: &Reactor) {
        Epoch::set_local_instance(self);

        // Create and set EpochCounter in thread local storage.
        //
        let epoch_counter = Box::leak(Box::new(EpochCounter::new()));
        EpochCounter::set_local_instance(epoch_counter);
    }

    // # Shutdown ordering
    //
    // `uninit_reactor` runs on each reactor's own thread after its event loop
    // returns (Coordinator line 142-148). No reactor calls `pin()` or
    // `bump_epoch()` during shutdown, so `min_local_epoch()` is never invoked
    // concurrently. This means the null-then-free sequence below cannot race
    // with a cross-thread reader.
    //
    fn uninit_reactor(&self, reactor: &Reactor) {
        // Null out the pointer in the counters array to prevent dangling access.
        //
        let reactor_index = reactor.reactor_index();
        let counters = Epoch::instance().counters.get().unwrap();
        // Null-out with Release ordering so that min_local_epoch() (which
        // loads with Acquire) sees null before we free the EpochCounter.
        counters[reactor_index].store(null_mut(), Ordering::Release);

        // Reclaim the leaked EpochCounter. Drop will drain all 3 garbage lists.
        // This is safe because during shutdown no other reactor holds references
        // to objects in our garbage bags (all event loops have returned).
        //
        let local_epoch_counter = EpochCounter::local_instance();
        // SAFETY: The EpochCounter was allocated via Box::leak in init_reactor.
        // We are the only thread with access (it's our thread-local counter),
        // and the counters[] pointer was already nulled above.
        unsafe {
            let _ = Box::from_raw(local_epoch_counter as *const _ as *mut EpochCounter);
        };

        Epoch::reset_local_instance();
    }
}

impl Default for Epoch {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Epoch {
    fn drop(&mut self) {}
}

// Intrusive node written into the freed allocation's memory.
// 16 bytes on 64-bit: next pointer (8) + size (4) + align (4).
// EpochAlloc enforces a minimum allocation of 16 bytes, so all types fit.
//
#[repr(C)]
pub(crate) struct DeferredAllocation {
    next: *mut DeferredAllocation,
    size: u32,
    align: u32,
}

// Intrusive singly-linked list of deferred deallocations.
//
struct DeferredList {
    head: *mut DeferredAllocation,
}

impl DeferredList {
    fn new() -> Self {
        DeferredList { head: null_mut() }
    }

    fn append(&mut self, ptr: *mut u8, layout: Layout) {
        let node = ptr as *mut DeferredAllocation;

        // SAFETY: `ptr` points to a previously-allocated object whose value
        // has been dropped. We reuse its memory for the intrusive list node.
        // EpochAlloc enforces a minimum allocation of 16 bytes, matching
        // size_of::<DeferredAllocation>() (next ptr + size u32 + align u32).
        unsafe {
            (*node).next = self.head;
            (*node).size = layout.size() as u32;
            (*node).align = layout.align() as u32;
        }

        self.head = node;
    }

    // Walks the list, deallocating each node, then resets the head.
    //
    fn reclaim(&mut self) {
        let mut node = self.head;
        while !node.is_null() {
            // SAFETY: Each node was written by `append` into a valid allocation.
            // We read fields before deallocating to avoid use-after-free.
            // The stored size/align were produced from Layout::new::<T>() in
            // deferred_delete, so they satisfy from_size_align's preconditions:
            // align is a power of two and size is a multiple of align (both
            // invariants of Layout, preserved through the u32 round-trip since
            // deferred_delete debug_asserts both fit in u32).
            //
            // Note: we store the *original* type layout, not the padded layout.
            // This is correct because dealloc routes through EpochAlloc::dealloc,
            // which re-pads the layout identically before forwarding to MiMalloc.
            unsafe {
                let next = (*node).next;
                let layout = Layout::from_size_align_unchecked(
                    (*node).size as usize,
                    (*node).align as usize,
                );
                dealloc(node as *mut u8, layout);
                node = next;
            }
        }

        self.head = null_mut();
    }
}

impl Drop for DeferredList {
    fn drop(&mut self) {
        self.reclaim();
    }
}

// Three deferred-deallocation lists cycling through epochs.
//
// Garbage tagged at epoch E goes into slot `E % 3`. When the global
// epoch reaches E+2, slot `(E+2 + 1) % 3 == (E % 3)` is reclaimed.
//
struct EpochGenerations {
    bags: [DeferredList; 3],
}

impl EpochGenerations {
    fn new() -> Self {
        EpochGenerations {
            bags: [
                DeferredList::new(),
                DeferredList::new(),
                DeferredList::new(),
            ],
        }
    }

    // Appends a deferred deallocation to the bag for the given epoch.
    //
    fn defer(&mut self, epoch: u32, ptr: *mut u8, layout: Layout) {
        self.bags[epoch as usize % 3].append(ptr, layout);
    }

    // Reclaims all garbage bags that have become safe since `old_epoch`.
    //
    // When transitioning from `old_epoch` to `current_epoch`, each epoch
    // advance E makes slot (E+1)%3 safe to reclaim. We replay all advances
    // to catch up, capped at 3 (all slots) to avoid redundant work.
    //
    fn reclaim(&mut self, old_epoch: u32, current_epoch: u32) {
        let advances = current_epoch.wrapping_sub(old_epoch).min(3);
        for i in 0..advances {
            let epoch = old_epoch.wrapping_add(1 + i);
            self.bags[(epoch as usize + 1) % 3].reclaim();
        }
    }
}

/// Per-reactor epoch state.
///
/// Tracks this reactor's observed epoch, pin nesting depth, and three
/// generation bags (one per epoch slot in the 3-state cycle).
///
pub(crate) struct EpochCounter {
    // Last global epoch observed by this reactor.
    // AtomicU32 because min_local_epoch() reads it cross-thread.
    //
    local_epoch: AtomicU32,

    // Pin nesting depth. Only accessed by the owning reactor thread.
    //
    pin_count: u32,

    // Generational garbage bags.
    //
    generations: EpochGenerations,
}

impl EpochCounter {
    pub(crate) fn new() -> Self {
        EpochCounter {
            local_epoch: AtomicU32::new(1),
            pin_count: 0,
            generations: EpochGenerations::new(),
        }
    }

    // Pins the current epoch.
    //
    // On the first pin (pin_count == 0), reads the global epoch. If it has
    // advanced, reclaims garbage from 2 epochs ago and updates local_epoch.
    //
    pub(crate) fn pin(&mut self) -> EpochGuard {
        if self.pin_count == 0 {
            let global = Epoch::instance()
                .current_epoch_index
                .load(Ordering::Acquire);

            let local = self.local_epoch.load(Ordering::Relaxed);
            if global != local {
                // Reclaim all garbage bags that became safe while we were away.
                //
                self.generations.reclaim(local, global);
                self.local_epoch.store(global, Ordering::Release);
            }
        }

        self.pin_count += 1;

        EpochGuard::new(self.local_epoch.load(Ordering::Relaxed))
    }

    pub(crate) fn unpin(&mut self) {
        debug_assert!(self.pin_count > 0);
        self.pin_count -= 1;
    }

    pub(crate) fn local_epoch(&self) -> u32 {
        self.local_epoch.load(Ordering::Relaxed)
    }

    pub(crate) fn local_instance<'a>() -> &'a mut Self {
        // SAFETY: Thread-local pointer is set by `set_local_instance` during
        // `init_reactor` and cleared by `reset_local_instance` in `uninit_reactor`.
        // Between those calls, the pointer is valid and exclusively owned by this thread.
        let epoch_counter_ptr =
            EPOCH_COUNTER_INSTANCE.with(|s| unsafe { *s.get() }) as *mut EpochCounter;

        unsafe { &mut *epoch_counter_ptr }
    }

    pub(super) fn set_local_instance(&self) {
        let reactor_index = Reactor::local_instance().reactor_index();
        let counters = Epoch::instance().counters.get().unwrap();
        // Store with Release ordering so that min_local_epoch() (which
        // loads with Acquire) sees a valid pointer.
        counters[reactor_index].store(self as *const _ as *mut EpochCounter, Ordering::Release);

        EPOCH_COUNTER_INSTANCE.with(|s| {
            // SAFETY: Single-threaded access to the thread-local cell.
            unsafe {
                *s.get() = self;
            };
        });
    }
}

impl Default for EpochCounter {
    fn default() -> Self {
        Self::new()
    }
}

// Epoch Thread Instance.
//
thread_local! {
    static EPOCH_INSTANCE: UnsafeCell<*const Epoch> = const {UnsafeCell::new(ptr::null_mut())};
}

// EpochCounter Thread Instance.
//
thread_local! {
    static EPOCH_COUNTER_INSTANCE: UnsafeCell<*const EpochCounter> = const {UnsafeCell::new(ptr::null_mut())};
}

// An Epoch Guard that keeps the current epoch pinned (in use).
//
// !Send + !Sync: the guard's Drop calls EpochCounter::local_instance().unpin(),
// which accesses the current thread's thread-local counter. Sending a guard to
// another thread would decrement the wrong thread's pin_count.
//
pub struct EpochGuard {
    epoch_index: u32,
    _not_send: PhantomData<*const ()>,
}

impl EpochGuard {
    pub(crate) fn new(epoch_index: u32) -> Self {
        EpochGuard {
            epoch_index,
            _not_send: PhantomData,
        }
    }

    pub fn epoch_index(&self) -> u32 {
        self.epoch_index
    }

    /// Re-observe the current global epoch while staying pinned.
    ///
    /// Returns the previously observed epoch index.
    pub fn repin(&mut self) -> u32 {
        let previous_epoch = self.epoch_index;
        let counter = EpochCounter::local_instance();
        let global_epoch = Epoch::instance()
            .current_epoch_index
            .load(Ordering::Acquire);

        if global_epoch != self.epoch_index {
            // Only the outermost pin may advance local_epoch and reclaim stale garbage.
            if counter.pin_count == 1 {
                counter.generations.reclaim(self.epoch_index, global_epoch);
                counter.local_epoch.store(global_epoch, Ordering::Release);
            }

            self.epoch_index = global_epoch;
        }

        previous_epoch
    }
}

impl Drop for EpochGuard {
    fn drop(&mut self) {
        EpochCounter::local_instance().unpin();
    }
}
