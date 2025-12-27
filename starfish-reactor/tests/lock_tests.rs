use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use rstest::rstest;
use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;
use starfish_reactor::cooperative_synchronization::lock::CooperativeFairLock;
use starfish_reactor::cooperative_synchronization::lock::CooperativeReactorAwareLock;
use starfish_reactor::cooperative_synchronization::lock::CooperativeUnfairLock;
use starfish_reactor::coordinator::Coordinator;
use starfish_reactor::reactor::Reactor;
use starfish_reactor::reactor::cooperative_yield;

// =============================================================================
// Lock trait for generic testing
// =============================================================================

trait TestLock: Clone + Send + Sync + 'static {
    fn new_lock(value: usize) -> Self;
    fn try_acquire_value(&self) -> Option<usize>;
    fn acquire_and_increment(&self) -> impl std::future::Future<Output = ()> + Send;
}

impl TestLock for CooperativeFairLock<usize> {
    fn new_lock(value: usize) -> Self {
        CooperativeFairLock::new(value)
    }

    fn try_acquire_value(&self) -> Option<usize> {
        self.try_acquire().map(|g| *g)
    }

    async fn acquire_and_increment(&self) {
        let mut guard = self.acquire().await;
        *guard += 1;
    }
}

impl TestLock for CooperativeUnfairLock<usize> {
    fn new_lock(value: usize) -> Self {
        CooperativeUnfairLock::new(value)
    }

    fn try_acquire_value(&self) -> Option<usize> {
        self.try_acquire().map(|g| *g)
    }

    async fn acquire_and_increment(&self) {
        let mut guard = self.acquire().await;
        *guard += 1;
    }
}

impl TestLock for CooperativeReactorAwareLock<usize> {
    fn new_lock(value: usize) -> Self {
        CooperativeReactorAwareLock::new(value)
    }

    fn try_acquire_value(&self) -> Option<usize> {
        self.try_acquire().map(|g| *g)
    }

    async fn acquire_and_increment(&self) {
        let mut guard = self.acquire().await;
        *guard += 1;
    }
}

// =============================================================================
// try_acquire Tests (FairLock only - other locks have same behavior)
// =============================================================================

#[test]
fn test_try_acquire_success() {
    let fair_lock = CooperativeFairLock::new(42);
    let guard = fair_lock.try_acquire();
    assert!(guard.is_some());
    assert_eq!(*guard.unwrap(), 42);
}

#[test]
fn test_try_acquire_failure_when_held() {
    let fair_lock = CooperativeFairLock::new(42);
    let _guard1 = fair_lock.try_acquire().unwrap();
    let guard2 = fair_lock.try_acquire();
    assert!(guard2.is_none());
}

#[test]
fn test_try_acquire_succeeds_after_release() {
    let fair_lock = CooperativeFairLock::new(42);
    {
        let _guard = fair_lock.try_acquire().unwrap();
    }
    let guard = fair_lock.try_acquire();
    assert!(guard.is_some());
}

// =============================================================================
// Guard Behavior Tests (FairLock only - other locks have same behavior)
// =============================================================================

#[test]
fn test_lock_guard_deref() {
    let fair_lock = CooperativeFairLock::new(vec![1, 2, 3]);
    let mut guard = fair_lock.try_acquire().unwrap();
    assert_eq!(guard.len(), 3);
    assert_eq!(guard[0], 1);
    guard.push(4);
    assert_eq!(guard.len(), 4);
}

#[test]
fn test_lock_released_on_guard_drop() {
    let fair_lock = CooperativeFairLock::new(42);
    {
        let guard = fair_lock.try_acquire();
        assert!(guard.is_some());
        assert!(fair_lock.try_acquire().is_none());
    }
    assert!(fair_lock.try_acquire().is_some());
}

// =============================================================================
// Single Task Tests
// =============================================================================

#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_single_task<L: TestLock>(#[case] lock: L) {
    let mut reactor = Reactor::new();
    let iterations = 50;

    let lock_clone = lock.clone();
    _ = reactor.spawn(async move {
        for _ in 0..iterations {
            lock_clone.acquire_and_increment().await;
            cooperative_yield().await;
        }
    });

    reactor.run();

    assert_eq!(lock.try_acquire_value().unwrap(), iterations);
}

// =============================================================================
// Two Tasks Contention Tests
// =============================================================================

#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_two_tasks_contention<L: TestLock>(#[case] lock: L) {
    let mut reactor = Reactor::new();
    let iterations = 50;

    let lock1 = lock.clone();
    let lock2 = lock.clone();

    _ = reactor.spawn(async move {
        for _ in 0..iterations {
            lock1.acquire_and_increment().await;
            cooperative_yield().await;
        }
    });

    _ = reactor.spawn(async move {
        for _ in 0..iterations {
            lock2.acquire_and_increment().await;
            cooperative_yield().await;
        }
    });

    reactor.run();

    assert_eq!(lock.try_acquire_value().unwrap(), iterations * 2);
}

// =============================================================================
// Many Tasks Contention Tests
// =============================================================================

#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_many_tasks_contention<L: TestLock>(#[case] lock: L) {
    let mut reactor = Reactor::new();
    let num_tasks = 10;
    let iterations_per_task = 20;

    for _ in 0..num_tasks {
        let lock = lock.clone();
        _ = reactor.spawn(async move {
            for _ in 0..iterations_per_task {
                lock.acquire_and_increment().await;
                cooperative_yield().await;
            }
        });
    }

    reactor.run();

    assert_eq!(
        lock.try_acquire_value().unwrap(),
        num_tasks * iterations_per_task
    );
}

// =============================================================================
// Cross-Reactor Tests
// =============================================================================

#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_cross_reactor<L: TestLock>(#[case] lock: L) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let iterations_per_reactor = 25;
    let num_reactors = 4;

    let done_event = Arc::new(CountdownEvent::new(num_reactors));

    for reactor_id in 0..num_reactors {
        let lock = lock.clone();
        let done = done_event.clone();

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_reactor {
                lock.acquire_and_increment().await;
                cooperative_yield().await;
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    assert_eq!(
        lock.try_acquire_value().unwrap(),
        num_reactors * iterations_per_reactor
    );
}

// =============================================================================
// Stress Tests
// =============================================================================

#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_stress<L: TestLock>(#[case] lock: L) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let counter = Arc::new(AtomicUsize::new(0));
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
                lock.acquire_and_increment().await;
                counter.fetch_add(1, Ordering::Relaxed);
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    assert_eq!(
        lock.try_acquire_value().unwrap(),
        num_tasks * iterations_per_task
    );
    assert_eq!(
        counter.load(Ordering::Relaxed),
        num_tasks * iterations_per_task
    );
}

// =============================================================================
// Extreme Stress Tests
// =============================================================================

/// Tests the race condition fix: waiter pushes after queue drain but before release.
/// Many tasks rapidly acquire/release with minimal work to maximize race window.
#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_rapid_acquire_release_race<L: TestLock>(#[case] lock: L) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(8);

    let num_tasks = 500;
    let iterations_per_task = 50;

    let done_event = Arc::new(CountdownEvent::new(num_tasks));

    for i in 0..num_tasks {
        let lock = lock.clone();
        let done = done_event.clone();
        let reactor_id = i % 8;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_task {
                // Minimal critical section to maximize contention.
                lock.acquire_and_increment().await;
                // No yield - immediately try to reacquire.
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    assert_eq!(
        lock.try_acquire_value().unwrap(),
        num_tasks * iterations_per_task
    );
}

/// Tests with all tasks starting simultaneously to maximize initial contention.
#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_thundering_herd<L: TestLock>(#[case] lock: L) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(8);

    let num_tasks = 200;
    let iterations_per_task = 25;

    // Barrier to synchronize all tasks starting together.
    let start_barrier = Arc::new(CountdownEvent::new(num_tasks));
    let done_event = Arc::new(CountdownEvent::new(num_tasks));

    for i in 0..num_tasks {
        let lock = lock.clone();
        let start = start_barrier.clone();
        let done = done_event.clone();
        let reactor_id = i % 8;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            // Signal ready and wait for all tasks.
            start.signal();

            for _ in 0..iterations_per_task {
                lock.acquire_and_increment().await;
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    assert_eq!(
        lock.try_acquire_value().unwrap(),
        num_tasks * iterations_per_task
    );
}

/// Tests mixed acquire patterns: some tasks yield, some don't.
/// This creates varied timing that can expose race conditions.
#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_mixed_yield_patterns<L: TestLock>(#[case] lock: L) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(8);

    let num_tasks = 200;
    let iterations_per_task = 30;

    let done_event = Arc::new(CountdownEvent::new(num_tasks));

    for i in 0..num_tasks {
        let lock = lock.clone();
        let done = done_event.clone();
        let reactor_id = i % 8;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for j in 0..iterations_per_task {
                lock.acquire_and_increment().await;

                // Varied yield patterns.
                match i % 4 {
                    0 => {} // Never yield.
                    1 => {
                        if j % 2 == 0 {
                            cooperative_yield().await;
                        }
                    }
                    2 => {
                        if j % 5 == 0 {
                            cooperative_yield().await;
                        }
                    }
                    _ => {
                        cooperative_yield().await;
                    }
                }
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    assert_eq!(
        lock.try_acquire_value().unwrap(),
        num_tasks * iterations_per_task
    );
}

/// Tests with tasks that hold the lock for varying durations.
/// Some do work inside the critical section, others release immediately.
#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_varied_hold_times<L: TestLock>(#[case] lock: L) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(8);

    let num_tasks = 150;
    let iterations_per_task = 20;

    let done_event = Arc::new(CountdownEvent::new(num_tasks));

    for i in 0..num_tasks {
        let lock = lock.clone();
        let done = done_event.clone();
        let reactor_id = i % 8;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_task {
                lock.acquire_and_increment().await;

                // Simulate varying work after release.
                let work_amount = i % 10;
                let mut dummy = 0usize;
                for k in 0..work_amount * 100 {
                    dummy = dummy.wrapping_add(k);
                }
                std::hint::black_box(dummy);
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    assert_eq!(
        lock.try_acquire_value().unwrap(),
        num_tasks * iterations_per_task
    );
}

/// Tests single reactor with many tasks - all acquire/release serialized.
#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_single_reactor_many_tasks<L: TestLock>(#[case] lock: L) {
    let mut reactor = Reactor::new();

    let num_tasks = 100;
    let iterations_per_task = 50;
    let completion_count = Arc::new(AtomicUsize::new(0));

    for _ in 0..num_tasks {
        let lock = lock.clone();
        let count = completion_count.clone();

        _ = reactor.spawn(async move {
            for _ in 0..iterations_per_task {
                lock.acquire_and_increment().await;
                cooperative_yield().await;
            }
            count.fetch_add(1, Ordering::Relaxed);
        });
    }

    reactor.run();

    assert_eq!(completion_count.load(Ordering::Relaxed), num_tasks);
    assert_eq!(
        lock.try_acquire_value().unwrap(),
        num_tasks * iterations_per_task
    );
}

/// Tests rapid lock hand-off between exactly 2 tasks on different reactors.
/// This is the minimal case for the race condition we fixed.
#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_two_reactor_ping_pong<L: TestLock>(#[case] lock: L) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(2);

    let iterations = 1000;

    let done_event = Arc::new(CountdownEvent::new(2));

    for reactor_id in 0..2 {
        let lock = lock.clone();
        let done = done_event.clone();

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations {
                lock.acquire_and_increment().await;
                // Immediate release and reacquire - maximizes race window.
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    assert_eq!(lock.try_acquire_value().unwrap(), 2 * iterations);
}

/// Tests with tasks dynamically spawning new tasks while competing for the lock.
#[rstest]
#[case::fair_lock(CooperativeFairLock::new_lock(0))]
#[case::unfair_lock(CooperativeUnfairLock::new_lock(0))]
#[case::reactor_aware_lock(CooperativeReactorAwareLock::new_lock(0))]
fn test_nested_spawn<L: TestLock>(#[case] lock: L) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let initial_tasks = 20;
    let spawns_per_task = 5;
    let iterations_per_task = 10;

    let total_tasks = initial_tasks + initial_tasks * spawns_per_task;
    let done_event = Arc::new(CountdownEvent::new(total_tasks));

    for i in 0..initial_tasks {
        let lock = lock.clone();
        let done = done_event.clone();
        let reactor_id = i % 4;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            // First, spawn child tasks.
            for j in 0..spawns_per_task {
                let child_lock = lock.clone();
                let child_done = done.clone();
                let child_reactor = (reactor_id + j + 1) % 4;

                drop(
                    Coordinator::reactor(child_reactor).spawn_external(async move {
                        for _ in 0..iterations_per_task {
                            child_lock.acquire_and_increment().await;
                        }
                        child_done.signal();
                    }),
                );
            }

            // Then do our own work.
            for _ in 0..iterations_per_task {
                lock.acquire_and_increment().await;
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    assert_eq!(
        lock.try_acquire_value().unwrap(),
        total_tasks * iterations_per_task
    );
}

/// Tests all three lock types under identical extreme conditions simultaneously.
#[test]
fn test_all_locks_extreme_stress() {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(8);

    let fair_lock = CooperativeFairLock::new(0usize);
    let unfair_lock = CooperativeUnfairLock::new(0usize);
    let reactor_lock = CooperativeReactorAwareLock::new(0usize);

    let num_tasks_per_lock = 100;
    let iterations_per_task = 30;
    let total_tasks = num_tasks_per_lock * 3;

    let done_event = Arc::new(CountdownEvent::new(total_tasks));

    // Spawn tasks for fair lock.
    for i in 0..num_tasks_per_lock {
        let lock = fair_lock.clone();
        let done = done_event.clone();
        let reactor_id = i % 8;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_task {
                let mut guard = lock.acquire().await;
                *guard += 1;
            }
            done.signal();
        }));
    }

    // Spawn tasks for unfair lock.
    for i in 0..num_tasks_per_lock {
        let lock = unfair_lock.clone();
        let done = done_event.clone();
        let reactor_id = i % 8;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_task {
                let mut guard = lock.acquire().await;
                *guard += 1;
            }
            done.signal();
        }));
    }

    // Spawn tasks for reactor-aware lock.
    for i in 0..num_tasks_per_lock {
        let lock = reactor_lock.clone();
        let done = done_event.clone();
        let reactor_id = i % 8;

        drop(Coordinator::reactor(reactor_id).spawn_external(async move {
            for _ in 0..iterations_per_task {
                let mut guard = lock.acquire().await;
                *guard += 1;
            }
            done.signal();
        }));
    }

    done_event.wait();
    coordinator.join_all().unwrap();

    let expected = num_tasks_per_lock * iterations_per_task;

    let fair_guard = fair_lock.try_acquire().unwrap();
    assert_eq!(*fair_guard, expected);
    drop(fair_guard);

    let unfair_guard = unfair_lock.try_acquire().unwrap();
    assert_eq!(*unfair_guard, expected);
    drop(unfair_guard);

    let reactor_guard = reactor_lock.try_acquire().unwrap();
    assert_eq!(*reactor_guard, expected);
}

/// Tests repeated spawn_external calls with the same coordinator.
/// This verifies the coordinator and locks work correctly under sustained load.
#[test]
fn test_repeated_cross_reactor_rounds() {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let lock = CooperativeFairLock::new(0usize);

    // Run many rounds to stress test the coordinator
    let num_rounds = 500;
    let ops_per_round = 1000; // 4 reactors * 250 ops each

    for round in 0..num_rounds {
        let done_event = Arc::new(CountdownEvent::new(4));

        for reactor_id in 0..4 {
            let lock = lock.clone();
            let done = done_event.clone();

            drop(Coordinator::reactor(reactor_id).spawn_external(async move {
                for _ in 0..250 {
                    let mut guard = lock.acquire().await;
                    *guard += 1;
                }
                done.signal();
            }));
        }

        done_event.wait();

        // Verify the lock value is correct after each round
        let expected = (round + 1) * ops_per_round;
        assert_eq!(
            lock.try_acquire().map(|g| *g),
            Some(expected),
            "Round {} failed: expected {}",
            round,
            expected
        );
    }

    coordinator.join_all().unwrap();

    assert_eq!(
        lock.try_acquire().map(|g| *g),
        Some(num_rounds * ops_per_round)
    );
}
