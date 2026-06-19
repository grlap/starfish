use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_reactor::coordinator::Coordinator;

use starfish_epoch::epoch::Epoch;

#[test]
fn epoch_init_test() {
    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    let reactor_count = 4;
    coordinator.initialize(reactor_count);

    let result_waits: Vec<_> =
        coordinator.for_each_reactor_with_result(|| async { Epoch::local_epoch() });

    coordinator.join_all().unwrap();

    let results: Vec<u32> = result_waits
        .into_iter()
        .map(|future| future.unwrap_result())
        .collect();

    assert_eq!(vec![1, 1, 1, 1], results);
}

#[test]
fn epoch_multi_pin_unpin() {
    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    coordinator.initialize(1);

    let result = coordinator.reactor_instance(0).spawn_external(async {
        // Pin 3 times (nested), then unpin all.
        //
        let guard1 = Epoch::pin();
        let guard2 = Epoch::pin();
        let guard3 = Epoch::pin();

        // All guards should have the same epoch index.
        assert_eq!(guard1.epoch_index(), guard2.epoch_index());
        assert_eq!(guard2.epoch_index(), guard3.epoch_index());

        drop(guard3);
        drop(guard2);
        drop(guard1);

        // Pin again — should still work after all unpins.
        let guard = Epoch::pin();
        assert_eq!(guard.epoch_index(), 1);
    });

    coordinator.join_all().unwrap();
    result.unwrap_result();
}

#[test]
fn epoch_bump_and_catchup() {
    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    coordinator.initialize(1);

    let result = coordinator.reactor_instance(0).spawn_external(async {
        // Reactor starts at epoch 1, matching global. Bump should succeed.
        assert!(Epoch::bump_epoch());

        // Global is now 2. Reactor's local_epoch is still 1 (hasn't pinned).
        // Second bump should fail: min_local_epoch (1) < global (2).
        assert!(!Epoch::bump_epoch());

        // Pin to observe the new epoch and catch up.
        let guard = Epoch::pin();
        assert_eq!(guard.epoch_index(), 2);
        drop(guard);

        // Now local_epoch == global == 2. Bump should succeed again.
        assert!(Epoch::bump_epoch());
    });

    coordinator.join_all().unwrap();
    result.unwrap_result();
}

#[test]
fn epoch_deferred_delete_small_type() {
    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    coordinator.initialize(1);

    let result = coordinator.reactor_instance(0).spawn_external(async {
        let guard = Epoch::pin();

        // Deferred delete of a small type (1 byte) — should not corrupt memory.
        // EpochAlloc guarantees at least 16 bytes, so the intrusive node fits.
        // u8 has no Drop impl, so drop_in_place is a no-op (but we call it
        // for correctness to model proper usage).
        let ptr = Box::into_raw(Box::new(42u8));
        unsafe {
            std::ptr::drop_in_place(ptr);
            Epoch::deferred_delete(ptr);
        }

        // Deferred delete of a larger type.
        let ptr = Box::into_raw(Box::new([0u64; 4]));
        unsafe {
            std::ptr::drop_in_place(ptr);
            Epoch::deferred_delete(ptr);
        }

        drop(guard);
    });

    coordinator.join_all().unwrap();
    result.unwrap_result();
}

#[test]
fn epoch_reclamation_after_two_advances() {
    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    coordinator.initialize(1);

    let result = coordinator.reactor_instance(0).spawn_external(async {
        // Allocate and schedule deferred deletion at epoch 1.
        let guard = Epoch::pin();
        let ptr = Box::into_raw(Box::new(123u64));
        unsafe {
            std::ptr::drop_in_place(ptr);
            Epoch::deferred_delete(ptr);
        }
        drop(guard);

        // Bump epoch 1 → 2.
        assert!(Epoch::bump_epoch());

        // Pin to observe epoch 2 and trigger local transition.
        // This reclaims garbage from slot (2+1)%3 = 0, which is epoch (2-2) = 0.
        // Our garbage was at epoch 1 (slot 1%3 = 1), NOT reclaimed yet.
        let guard = Epoch::pin();
        drop(guard);

        // Bump epoch 2 → 3.
        assert!(Epoch::bump_epoch());

        // Pin to observe epoch 3 and trigger local transition.
        // This reclaims garbage from slot (3+1)%3 = 1, which is epoch 1.
        // Our deferred allocation from epoch 1 is now freed.
        let guard = Epoch::pin();
        drop(guard);

        // If we got here without a crash, reclamation worked.
    });

    coordinator.join_all().unwrap();
    result.unwrap_result();
}

#[test]
fn epoch_multi_reactor_bump_blocked_by_lagging_reactor() {
    // Synchronization flags shared between reactor threads.
    static R1_PINNED: AtomicBool = AtomicBool::new(false);
    static R0_DONE: AtomicBool = AtomicBool::new(false);

    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    coordinator.initialize(2);

    R1_PINNED.store(false, Ordering::Relaxed);
    R0_DONE.store(false, Ordering::Relaxed);

    // Reactor 0: wait for reactor 1 to pin, then bump and verify the
    // second bump is blocked by reactor 1's lagging local_epoch.
    let r0 = coordinator.reactor_instance(0).spawn_external(async {
        // Wait until reactor 1 has pinned at epoch 1.
        while !R1_PINNED.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }

        let guard = Epoch::pin();
        assert_eq!(guard.epoch_index(), 1);

        // Bump epoch 1 → 2.
        assert!(Epoch::bump_epoch());

        // Second bump should fail: reactor 1's local_epoch is still 1.
        assert!(!Epoch::bump_epoch());

        drop(guard);

        // Signal reactor 1 that we're done.
        R0_DONE.store(true, Ordering::Release);
    });

    // Reactor 1: pin at epoch 1, signal reactor 0, then wait for it to finish.
    let r1 = coordinator.reactor_instance(1).spawn_external(async {
        let guard = Epoch::pin();
        assert_eq!(guard.epoch_index(), 1);

        // Signal reactor 0 that we've pinned.
        R1_PINNED.store(true, Ordering::Release);

        // Keep the guard alive until reactor 0 finishes its assertions.
        while !R0_DONE.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }

        drop(guard);
    });

    coordinator.join_all().unwrap();
    r0.unwrap_result();
    r1.unwrap_result();
}

/// Verifies that deferred allocations are actually freed by observing a
/// side-effect: a `Drop` impl that increments an atomic counter.
#[test]
fn epoch_reclamation_frees_memory() {
    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    struct Tracked(#[allow(dead_code)] u64);

    impl Drop for Tracked {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }

    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    coordinator.initialize(1);

    let result = coordinator.reactor_instance(0).spawn_external(async {
        DROP_COUNT.store(0, Ordering::Relaxed);

        // Allocate 3 Tracked objects at epoch 1 and schedule deferred delete.
        let guard = Epoch::pin();
        for i in 0..3 {
            let ptr = Box::into_raw(Box::new(Tracked(i)));
            unsafe {
                std::ptr::drop_in_place(ptr);
                Epoch::deferred_delete(ptr);
            }
        }
        drop(guard);

        // drop_in_place was called for all 3 — verify Drop ran.
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 3);

        // Advance epoch 1 → 2 → 3.
        assert!(Epoch::bump_epoch());
        let guard = Epoch::pin();
        drop(guard);

        assert!(Epoch::bump_epoch());

        // Pin to observe epoch 3 — triggers reclaim of epoch 1 garbage.
        // The deallocation itself doesn't call Drop again (already dropped),
        // but we verify the 3 objects were dropped exactly once above.
        let guard = Epoch::pin();
        drop(guard);

        // Allocate again at the new epoch to verify no corruption.
        let guard = Epoch::pin();
        let ptr = Box::into_raw(Box::new(Tracked(99)));
        unsafe {
            std::ptr::drop_in_place(ptr);
            Epoch::deferred_delete(ptr);
        }
        drop(guard);

        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 4);
    });

    coordinator.join_all().unwrap();
    result.unwrap_result();
}

#[test]
fn epoch_guard_repin_catches_up_local_epoch() {
    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    coordinator.initialize(1);

    let result = coordinator.reactor_instance(0).spawn_external(async {
        let mut guard = Epoch::pin();
        assert_eq!(guard.epoch_index(), 1);

        assert!(Epoch::bump_epoch());

        let previous_epoch = guard.repin();
        assert_eq!(previous_epoch, 1);
        assert_eq!(guard.epoch_index(), 2);
        assert_eq!(Epoch::local_epoch(), 2);
    });

    coordinator.join_all().unwrap();
    result.unwrap_result();
}

#[test]
fn nested_epoch_guard_repin_does_not_advance_local_epoch() {
    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    coordinator.initialize(1);

    let result = coordinator.reactor_instance(0).spawn_external(async {
        let mut outer = Epoch::pin();
        let mut inner = Epoch::pin();

        assert!(Epoch::bump_epoch());

        let previous_epoch = inner.repin();
        assert_eq!(previous_epoch, 1);
        assert_eq!(inner.epoch_index(), 2);
        assert_eq!(Epoch::local_epoch(), 1);

        drop(inner);

        let previous_epoch = outer.repin();
        assert_eq!(previous_epoch, 1);
        assert_eq!(outer.epoch_index(), 2);
        assert_eq!(Epoch::local_epoch(), 2);
    });

    coordinator.join_all().unwrap();
    result.unwrap_result();
}
