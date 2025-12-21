use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;
use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_reactor::cooperative_synchronization::lock::CooperativeFairLock;
use starfish_reactor::cooperative_synchronization::lock::CooperativeReactorAwareLock;
use starfish_reactor::cooperative_synchronization::lock::CooperativeUnfairLock;
use starfish_reactor::coordinator::Coordinator;
use starfish_reactor::reactor::Reactor;
use starfish_reactor::reactor::cooperative_yield;

// =============================================================================
// UnfairLock Tests
// =============================================================================

#[test]
fn test_unfair_lock_single_task() {
    // Single task acquiring lock repeatedly (no contention).
    //
    let mut coordinator = Coordinator::new();
    _ = coordinator.initialize(4);

    let unfair_lock = CooperativeUnfairLock::new(RefCell::new(1));

    let n = 100;

    let result_wait =
        Coordinator::reactor(0).spawn_external_with_result(compute_unfair(n, unfair_lock.clone()));

    _ = coordinator.join_all();

    let result = result_wait.unwrap_result();
    assert!(result);

    let result = unfair_lock.try_acquire().unwrap();
    let value = *result.borrow();

    assert_eq!(n + 1, value);
}

async fn compute_unfair(n: i32, unfair_lock: CooperativeUnfairLock<RefCell<i32>>) -> bool {
    for _ in 0..n {
        {
            let lock_result = unfair_lock.acquire().await;

            *(*lock_result).borrow_mut() += 1;
        }

        cooperative_yield().await;
    }

    true
}

// =============================================================================
// FairLock Tests
// =============================================================================

#[test]
fn test_fair_lock_single_task() {
    // Single task acquiring FairLock repeatedly (no contention).
    //
    let mut coordinator = Coordinator::new();
    _ = coordinator.initialize(1);

    let fair_lock = CooperativeFairLock::new(RefCell::new(0));

    let n = 100;

    let result_wait =
        Coordinator::reactor(0).spawn_external_with_result(compute_fair(n, fair_lock.clone()));

    _ = coordinator.join_all();

    let result = result_wait.unwrap_result();
    assert!(result);

    let guard = fair_lock.try_acquire().unwrap();
    let value = *guard.borrow();

    assert_eq!(n, value);
}

async fn compute_fair(n: i32, fair_lock: CooperativeFairLock<RefCell<i32>>) -> bool {
    for _ in 0..n {
        {
            let lock_result = fair_lock.acquire().await;
            *(*lock_result).borrow_mut() += 1;
        }

        cooperative_yield().await;
    }

    true
}

// =============================================================================
// try_acquire Tests
// =============================================================================

#[test]
fn test_try_acquire_success() {
    // try_acquire should succeed when lock is not held.
    //
    let fair_lock = CooperativeFairLock::new(42);

    let guard = fair_lock.try_acquire();
    assert!(guard.is_some());
    assert_eq!(*guard.unwrap(), 42);
}

#[test]
fn test_try_acquire_failure_when_held() {
    // try_acquire should fail when lock is already held.
    //
    let fair_lock = CooperativeFairLock::new(42);

    let _guard1 = fair_lock.try_acquire().unwrap();

    // Second try_acquire should fail.
    //
    let guard2 = fair_lock.try_acquire();
    assert!(guard2.is_none());
}

#[test]
fn test_try_acquire_succeeds_after_release() {
    // try_acquire should succeed after lock is released.
    //
    let fair_lock = CooperativeFairLock::new(42);

    {
        let _guard = fair_lock.try_acquire().unwrap();
        // Lock held here.
    }
    // Lock released here (guard dropped).

    // Should succeed now.
    //
    let guard = fair_lock.try_acquire();
    assert!(guard.is_some());
}

// =============================================================================
// Contention Tests
// =============================================================================

#[test]
fn test_fair_lock_two_tasks_contention() {
    // Two tasks on same reactor competing for FairLock.
    // This tests the waiter queue and signaling mechanism.
    //
    let mut reactor = Reactor::new();

    let fair_lock = CooperativeFairLock::new(0i32);
    let iterations = 50;

    let lock1 = fair_lock.clone();
    let lock2 = fair_lock.clone();

    // Task 1: increment by 1.
    //
    _ = reactor.spawn(async move {
        for _ in 0..iterations {
            {
                let mut guard = lock1.acquire().await;
                *guard += 1;
            }
            cooperative_yield().await;
        }
    });

    // Task 2: increment by 1.
    //
    _ = reactor.spawn(async move {
        for _ in 0..iterations {
            {
                let mut guard = lock2.acquire().await;
                *guard += 1;
            }
            cooperative_yield().await;
        }
    });

    reactor.run();

    let guard = fair_lock.try_acquire().unwrap();
    assert_eq!(*guard, iterations * 2);
}

#[test]
fn test_unfair_lock_two_tasks_contention() {
    // Two tasks on same reactor competing for UnfairLock.
    //
    let mut reactor = Reactor::new();

    let unfair_lock = CooperativeUnfairLock::new(0i32);
    let iterations = 50;

    let lock1 = unfair_lock.clone();
    let lock2 = unfair_lock.clone();

    // Task 1.
    //
    _ = reactor.spawn(async move {
        for _ in 0..iterations {
            {
                let mut guard = lock1.acquire().await;
                *guard += 1;
            }
            cooperative_yield().await;
        }
    });

    // Task 2.
    //
    _ = reactor.spawn(async move {
        for _ in 0..iterations {
            {
                let mut guard = lock2.acquire().await;
                *guard += 1;
            }
            cooperative_yield().await;
        }
    });

    reactor.run();

    let guard = unfair_lock.try_acquire().unwrap();
    assert_eq!(*guard, iterations * 2);
}

#[test]
fn test_fair_lock_many_tasks_contention() {
    // Many tasks on same reactor competing for FairLock.
    //
    let mut reactor = Reactor::new();

    let fair_lock = CooperativeFairLock::new(0i32);
    let num_tasks = 10;
    let iterations_per_task = 20;

    for _ in 0..num_tasks {
        let lock = fair_lock.clone();
        _ = reactor.spawn(async move {
            for _ in 0..iterations_per_task {
                {
                    let mut guard = lock.acquire().await;
                    *guard += 1;
                }
                cooperative_yield().await;
            }
        });
    }

    reactor.run();

    let guard = fair_lock.try_acquire().unwrap();
    assert_eq!(*guard, num_tasks * iterations_per_task);
}

// =============================================================================
// Cross-Reactor Tests
// =============================================================================

#[test]
fn test_fair_lock_cross_reactor() {
    // Tasks on different reactors competing for the same lock.
    //
    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let fair_lock = CooperativeFairLock::new(0i32);
    let iterations_per_reactor = 25;
    let num_reactors = 4;

    let done_event = Arc::new(CountdownEvent::new(num_reactors));

    for reactor_id in 0..num_reactors {
        let lock = fair_lock.clone();
        let done = done_event.clone();

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_reactor {
                {
                    let mut guard = lock.acquire().await;
                    *guard += 1;
                }
                cooperative_yield().await;
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    let guard = fair_lock.try_acquire().unwrap();
    assert_eq!(*guard, num_reactors as i32 * iterations_per_reactor);
}

#[test]
fn test_unfair_lock_cross_reactor() {
    // Tasks on different reactors competing for UnfairLock.
    //
    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let unfair_lock = CooperativeUnfairLock::new(0i32);
    let iterations_per_reactor = 25;
    let num_reactors = 4;

    let done_event = Arc::new(CountdownEvent::new(num_reactors));

    for reactor_id in 0..num_reactors {
        let lock = unfair_lock.clone();
        let done = done_event.clone();

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_reactor {
                {
                    let mut guard = lock.acquire().await;
                    *guard += 1;
                }
                cooperative_yield().await;
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    let guard = unfair_lock.try_acquire().unwrap();
    assert_eq!(*guard, num_reactors as i32 * iterations_per_reactor);
}

// =============================================================================
// Stress Tests
// =============================================================================

#[test]
fn test_fair_lock_stress() {
    // Stress test with many tasks and iterations.
    //
    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let counter = Arc::new(AtomicUsize::new(0));
    let fair_lock = CooperativeFairLock::new(0usize);
    let num_tasks = 100;
    let iterations_per_task = 10;

    let done_event = Arc::new(CountdownEvent::new(num_tasks));

    for i in 0..num_tasks {
        let lock = fair_lock.clone();
        let done = done_event.clone();
        let counter = counter.clone();
        let reactor_id = i % 4;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_task {
                {
                    let mut guard = lock.acquire().await;
                    *guard += 1;
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                // Occasionally yield to increase contention variety.
                //
                if i % 3 == 0 {
                    cooperative_yield().await;
                }
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    let guard = fair_lock.try_acquire().unwrap();
    assert_eq!(*guard, num_tasks * iterations_per_task);
    assert_eq!(
        counter.load(Ordering::Relaxed),
        num_tasks * iterations_per_task
    );
}

#[test]
fn test_unfair_lock_stress() {
    // Stress test for UnfairLock.
    //
    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let counter = Arc::new(AtomicUsize::new(0));
    let unfair_lock = CooperativeUnfairLock::new(0usize);
    let num_tasks = 100;
    let iterations_per_task = 10;

    let done_event = Arc::new(CountdownEvent::new(num_tasks));

    for i in 0..num_tasks {
        let lock = unfair_lock.clone();
        let done = done_event.clone();
        let counter = counter.clone();
        let reactor_id = i % 4;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_task {
                {
                    let mut guard = lock.acquire().await;
                    *guard += 1;
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                if i % 3 == 0 {
                    cooperative_yield().await;
                }
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    let guard = unfair_lock.try_acquire().unwrap();
    assert_eq!(*guard, num_tasks * iterations_per_task);
    assert_eq!(
        counter.load(Ordering::Relaxed),
        num_tasks * iterations_per_task
    );
}

// =============================================================================
// Guard Behavior Tests
// =============================================================================

#[test]
fn test_lock_guard_deref() {
    // Test Deref/DerefMut on lock guard.
    //
    let fair_lock = CooperativeFairLock::new(vec![1, 2, 3]);

    let mut guard = fair_lock.try_acquire().unwrap();

    // Deref.
    //
    assert_eq!(guard.len(), 3);
    assert_eq!(guard[0], 1);

    // DerefMut.
    //
    guard.push(4);
    assert_eq!(guard.len(), 4);
}

#[test]
fn test_lock_released_on_guard_drop() {
    // Verify lock is released when guard is dropped.
    //
    let fair_lock = CooperativeFairLock::new(42);

    {
        let guard = fair_lock.try_acquire();
        assert!(guard.is_some());

        // Lock should be held.
        //
        assert!(fair_lock.try_acquire().is_none());
    }

    // Lock should be released after guard dropped.
    //
    assert!(fair_lock.try_acquire().is_some());
}

// =============================================================================
// ReactorAwareLock Tests
// =============================================================================

#[test]
fn test_reactor_aware_lock_single_task() {
    // Single task acquiring ReactorAwareLock repeatedly (no contention).
    //
    let mut reactor = Reactor::new();

    let lock = CooperativeReactorAwareLock::new(0i32);
    let iterations = 50;

    let lock_clone = lock.clone();
    _ = reactor.spawn(async move {
        for _ in 0..iterations {
            {
                let mut guard = lock_clone.acquire().await;
                *guard += 1;
            }
            cooperative_yield().await;
        }
    });

    reactor.run();

    let guard = lock.try_acquire().unwrap();
    assert_eq!(*guard, iterations);
}

#[test]
fn test_reactor_aware_lock_two_tasks_contention() {
    // Two tasks on same reactor competing for ReactorAwareLock.
    //
    let mut reactor = Reactor::new();

    let lock = CooperativeReactorAwareLock::new(0i32);
    let iterations = 50;

    let lock1 = lock.clone();
    let lock2 = lock.clone();

    _ = reactor.spawn(async move {
        for _ in 0..iterations {
            {
                let mut guard = lock1.acquire().await;
                *guard += 1;
            }
            cooperative_yield().await;
        }
    });

    _ = reactor.spawn(async move {
        for _ in 0..iterations {
            {
                let mut guard = lock2.acquire().await;
                *guard += 1;
            }
            cooperative_yield().await;
        }
    });

    reactor.run();

    let guard = lock.try_acquire().unwrap();
    assert_eq!(*guard, iterations * 2);
}

#[test]
fn test_reactor_aware_lock_cross_reactor() {
    // Tasks on different reactors competing for ReactorAwareLock.
    // The lock should prefer waking tasks on less loaded reactors.
    //
    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let lock = CooperativeReactorAwareLock::new(0i32);
    let iterations_per_reactor = 25;
    let num_reactors = 4;

    let done_event = Arc::new(CountdownEvent::new(num_reactors));

    for reactor_id in 0..num_reactors {
        let lock = lock.clone();
        let done = done_event.clone();

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_reactor {
                {
                    let mut guard = lock.acquire().await;
                    *guard += 1;
                }
                cooperative_yield().await;
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    let guard = lock.try_acquire().unwrap();
    assert_eq!(*guard, num_reactors as i32 * iterations_per_reactor);
}

#[test]
fn test_reactor_aware_lock_stress() {
    // Stress test for ReactorAwareLock.
    //
    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let counter = Arc::new(AtomicUsize::new(0));
    let lock = CooperativeReactorAwareLock::new(0usize);
    let num_tasks = 100;
    let iterations_per_task = 10;

    let done_event = Arc::new(CountdownEvent::new(num_tasks));

    for i in 0..num_tasks {
        let lock = lock.clone();
        let done = done_event.clone();
        let counter = counter.clone();
        let reactor_id = i % 4;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_task {
                {
                    let mut guard = lock.acquire().await;
                    *guard += 1;
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                if i % 3 == 0 {
                    cooperative_yield().await;
                }
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    let guard = lock.try_acquire().unwrap();
    assert_eq!(*guard, num_tasks * iterations_per_task);
    assert_eq!(
        counter.load(Ordering::Relaxed),
        num_tasks * iterations_per_task
    );
}
