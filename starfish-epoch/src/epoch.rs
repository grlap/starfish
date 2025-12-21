use std::alloc::{Layout, dealloc};
use std::cell::UnsafeCell;
use std::cmp::min;
use std::ptr::{self, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};

use starfish_reactor::coordinator::{CooperativeIntitializer, Coordinator};
use starfish_reactor::reactor::Reactor;

pub struct Epoch {
    current_epoch_index: AtomicU32,
    counters: Vec<UnsafeCell<*const EpochCounter>>,
}

unsafe impl Send for Epoch {}
unsafe impl Sync for Epoch {}

impl Epoch {
    pub fn new() -> Self {
        Epoch {
            current_epoch_index: AtomicU32::new(1),
            counters: vec![],
        }
    }

    pub fn uninit(&mut self) {}

    // Pins the current Epoch (Epoch is use).
    //
    pub fn pin() -> EpochGuard {
        // Get a Epoch counter for a current reactor.
        //
        let epoch_counter = EpochCounter::local_instance();

        epoch_counter.pin()
    }

    pub fn unpin(pinned_epoch: &EpochGuard) {
        let epoch_counter = EpochCounter::local_instance();

        epoch_counter.unpin(pinned_epoch)
    }

    pub fn bump_epoch() -> bool {
        let epoch = Epoch::instance();

        let still_in_use_epoch_index = Self::still_in_use_epoch_index();

        let epoch_index = epoch
            .current_epoch_index
            .load(std::sync::atomic::Ordering::Acquire);

        if epoch_index != still_in_use_epoch_index {
            // Some reactors are still using previous Epoch.
            //
            return false;
        }

        // All reactors are using the current Epoch. It is safe to increase the Epoch.
        //
        epoch
            .current_epoch_index
            .compare_exchange(
                epoch_index,
                epoch_index + 1,
                Ordering::Acquire,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    }

    // Gets the Epoch Index which is still in use.
    //
    pub(crate) fn still_in_use_epoch_index() -> u32 {
        let coordinator = Coordinator::instance();

        let reactor_count = coordinator.reactor_count();

        let mut epoch_index = EpochCounter::instance(0).retired_epoch_index;

        for reactor_index in 1..reactor_count {
            epoch_index = min(
                epoch_index,
                EpochCounter::instance(reactor_index).retired_epoch_index,
            );
        }

        epoch_index
    }

    pub fn instance<'a>() -> &'a mut Self {
        let epoch_ptr = EPOCH_INSTANCE.with(|s| unsafe { *s.get() }) as *mut Epoch;

        unsafe { &mut *epoch_ptr }
    }

    pub(super) fn set_local_instance(&self) {
        EPOCH_INSTANCE.with(|s| {
            unsafe {
                *s.get() = self;
            };
        });
    }

    pub(super) fn reset_local_instance() {
        EPOCH_INSTANCE.with(|s| {
            unsafe {
                *s.get() = ptr::null_mut();
            };
        });
    }

    // Adds allocation into the deffered list.
    //
    pub fn deffered_delete<T>(object: Box<T>) {
        EpochCounter::local_instance()
            .deffered_delete(Box::into_raw(object) as *mut u8, Layout::new::<T>())
    }
}

impl CooperativeIntitializer for Epoch {
    // Initialize Epoch Framework.
    //
    fn init(&mut self) {
        let coordinator = Coordinator::instance();

        // Create EpochCounters
        //
        self.counters = (0..coordinator.reactor_count())
            .map(|_| UnsafeCell::new(ptr::null()))
            .collect();
    }

    fn uninit(&mut self) {
        todo!()
    }

    fn init_reactor(&self, _: &Reactor) {
        Epoch::set_local_instance(self);

        // Create and set EpochCounter in thread local storage.
        //
        let epoch_counter = Box::leak(Box::new(EpochCounter::new()));
        EpochCounter::set_local_instance(epoch_counter);
    }

    fn uninit_reactor(&self, _: &Reactor) {
        let local_epoch_counter = EpochCounter::local_instance();
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
    fn drop(&mut self) {
        // Uninitialize Epoch.
        //
        self.uninit();
    }
}

struct DefferedAllocationsList {
    // Any other reclaimed allocations.
    //
    deffered_allocations_ptr: *mut AllocationPointer,
}

impl DefferedAllocationsList {
    fn new() -> Self {
        DefferedAllocationsList {
            deffered_allocations_ptr: null_mut(),
        }
    }

    fn append(&mut self, ptr: *mut u8, layout: Layout) {
        let deffered_allocation_ptr = ptr as *mut AllocationPointer;

        unsafe {
            (*deffered_allocation_ptr).next = self.deffered_allocations_ptr;
            (*deffered_allocation_ptr).layout = layout;
        }

        self.deffered_allocations_ptr = deffered_allocation_ptr;
    }
}

impl Drop for DefferedAllocationsList {
    fn drop(&mut self) {
        // Free allocatiobns.
        //
        let mut deffered_allocation_ptr = self.deffered_allocations_ptr;
        while !deffered_allocation_ptr.is_null() {
            unsafe {
                let next_deffered_allocation_ptr = (*deffered_allocation_ptr).next;
                dealloc(
                    deffered_allocation_ptr as *mut u8,
                    (*deffered_allocation_ptr).layout,
                );

                deffered_allocation_ptr = next_deffered_allocation_ptr;
            }
        }

        self.deffered_allocations_ptr = null_mut();
    }
}

#[repr(C)]
struct AllocationPointer {
    next: *mut AllocationPointer,
    layout: Layout,
}

/// Defines the Epoch usage.
///
pub struct EpochCounter {
    // Index of the retired Epoch. Always set, when Epoch bump operation is in progress it equals active Epoch index minus one othewise it is equal to active Epoch index.
    //
    retired_epoch_index: u32,

    // Index of the active Epoch.
    //
    active_epoch_index: u32,

    // The usage counter for the retired Epoch. Used only when Epoch bump operation is in progress.
    //
    retired_use_counter: i32,

    // The usage counter for the active Epoch.
    //
    active_counter: i32,

    // Any other reclaimed allocations.
    //
    deffered_allocations_list: DefferedAllocationsList,
}

impl EpochCounter {
    pub fn new() -> Self {
        EpochCounter {
            retired_epoch_index: 1,
            active_epoch_index: 1,
            retired_use_counter: 0,
            active_counter: 0,
            deffered_allocations_list: DefferedAllocationsList::new(),
        }
    }

    // Pins the current Epoch.
    //
    pub fn pin(&mut self) -> EpochGuard {
        // Check if we need to increase the epoch.
        //
        if self.retired_epoch_index != self.active_epoch_index {
            // Bump the epoch.
            //
            self.retired_use_counter = self.active_counter;
            self.active_counter = 0
        }

        // Increase usage.
        //
        self.active_counter += 1;

        EpochGuard::new(self.active_epoch_index)
    }

    pub fn unpin(&mut self, pinned_epoch: &EpochGuard) {
        match self.active_epoch_index == pinned_epoch.epoch_index {
            true => {
                // We are still on the same Epoch.
                //
                self.active_counter = -1;
            }
            false => {
                // New Epoch. Decrease the previous Epoch counter.
                //
                self.retired_use_counter = -1;

                if self.retired_use_counter == 0 {
                    // Update the old Epoch is no longer used for this Reactor.
                    //
                    self.retired_epoch_index = self.active_epoch_index;
                }
            }
        }
    }

    fn deffered_delete(&mut self, ptr: *mut u8, layout: Layout) {
        self.deffered_allocations_list.append(ptr, layout);
    }

    pub fn active_epoch_index(&self) -> u32 {
        self.active_epoch_index
    }

    pub fn instance<'a>(reactor_index: usize) -> &'a mut Self {
        let epoch_counter_ptr =
            *Epoch::instance().counters[reactor_index].get_mut() as *mut EpochCounter;

        unsafe { &mut *epoch_counter_ptr }
    }

    pub fn local_instance<'a>() -> &'a mut Self {
        let epoch_counter_ptr =
            EPOCH_COUNTER_INSTANCE.with(|s| unsafe { *s.get() }) as *mut EpochCounter;

        unsafe { &mut *epoch_counter_ptr }
    }

    pub(super) fn set_local_instance(&self) {
        let reactor_index = Reactor::local_instance().reactor_index();
        unsafe {
            *Epoch::instance().counters[reactor_index].get() = self;
        }

        EPOCH_COUNTER_INSTANCE.with(|s| {
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

impl Drop for EpochCounter {
    fn drop(&mut self) {}
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

// An Epoch Guard that keeps the current Epoch pinned (used).
//
pub struct EpochGuard {
    epoch_index: u32,
}

impl EpochGuard {
    pub(crate) fn new(epoch_index: u32) -> Self {
        EpochGuard { epoch_index }
    }
}

impl Drop for EpochGuard {
    fn drop(&mut self) {
        Epoch::unpin(self);
    }
}
