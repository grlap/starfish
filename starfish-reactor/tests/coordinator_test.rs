use std::{future::Future, sync::Arc, thread};

use starfish_core::preemptive_synchronization::{
    countdown_event::CountdownEvent, future_extension::FutureExtension,
};
use starfish_reactor::coordinator::Coordinator;
use starfish_reactor::reactor::{CooperativeReactor, Reactor, cooperative_yield};

/// Tests the basic lifecycle of a Coordinator instance:
/// 1. Creates a new Coordinator
/// 2. Initializes it with 2 workers
/// 3. Verifies that join_all() completes successfully, ensuring all workers terminate properly
///
/// This test checks the fundamental functionality of creating, initializing, and
/// gracefully shutting down a Coordinator without any errors.
///
#[test]
fn test_create_coordinator() {
    let mut coordinator = Coordinator::new();
    _ = coordinator.initialize(2);

    _ = coordinator.join_all();
}

async fn compute(n: char) -> i32 {
    for i in 0..10 {
        println!("{:?}[.] {} {}", thread::current().id(), n, i);
        cooperative_yield().await;
    }

    println!("{:?}[x] {} []", thread::current().id(), n);
    1
}

async fn compute_test() {
    let result_future_1 = Reactor::local_instance().spawn_with_result(compute('A'));
    let result_future_2 = Reactor::local_instance().spawn_with_result(compute('B'));

    let result_1 = result_future_1.await;
    let result_2 = result_future_2.await;

    assert_eq!(1, result_1);
    assert_eq!(1, result_2);
}

#[test]
fn test_create_coordinator2() {
    let mut cooperative_scheduler = CooperativeReactor::new(Reactor::new());
    cooperative_scheduler.set_is_in_shutdown_flag(false);

    let cooperative_scheduler_clone = cooperative_scheduler.clone();

    _ = cooperative_scheduler.spawn(async move {
        // Test.
        //
        let mut coordinator = Coordinator::new();
        _ = coordinator.initialize(2);

        let w1 = Coordinator::reactor(0).spawn_external(compute_test());
        let w2 = Coordinator::reactor(1).spawn_external(compute_test());

        w1.await;
        w2.await;

        _ = coordinator.join_all();
        cooperative_scheduler_clone.set_is_in_shutdown_flag(true);
    });

    cooperative_scheduler.run();
}

async fn execute_async(countdown_event: Arc<CountdownEvent>) {
    let a = Coordinator::reactor(1).spawn_external(compute_test());
    let b = Coordinator::reactor(2).spawn_external(compute_test());
    let c = Coordinator::reactor(3).spawn_external(compute_test());

    // Test.
    a.await;
    b.await;
    c.await;

    countdown_event.signal();
}

#[test]
fn test_execute_async() {
    let countdown_event = Arc::new(CountdownEvent::new(1));

    // Test.
    //
    let mut coordinator = Coordinator::new();
    _ = coordinator.initialize(4);

    // #TODO, implement without countdown_event
    //
    let wait = Coordinator::reactor(0).spawn_external(execute_async(countdown_event.clone()));

    countdown_event.wait();

    _ = coordinator.join_all();

    wait.unwrap_result();
}

#[test]
fn test_spawn_external() {
    // Test.
    //
    let mut coordinator = Coordinator::new();
    _ = coordinator.initialize(1);

    let result_wait = Coordinator::reactor(0).spawn_external_with_result(compute('|'));

    _ = coordinator.join_all();

    let result = result_wait.unwrap_result();
    assert_eq!(1, result);
}

fn fibonacci_spawn_external(n: u64) -> impl Future<Output = u64> + Send {
    async move {
        match n {
            0 => 1,
            1 => 1,
            n => {
                let result_future_1 = Coordinator::instance()
                    .reactor_instance(0)
                    .spawn_external_with_result(fibonacci_spawn_external(n - 1));
                let result_future_2 = Coordinator::instance()
                    .reactor_instance(0)
                    .spawn_external_with_result(fibonacci_spawn_external(n - 2));

                let n_1 = Box::pin(result_future_1).await;
                let n_2 = Box::pin(result_future_2).await;
                n_1 + n_2
            }
        }
    }
}

#[test]
fn fibonacci_spawn_external_test() {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(1);

    let reactor = coordinator.reactor_instance(0);

    let result_wait = reactor.spawn_external_with_result(async {
        Reactor::local_instance()
            .spawn_with_result(fibonacci_spawn_external(20))
            .await
    });

    coordinator.join_all().unwrap();

    assert_eq!(10946, result_wait.unwrap_result());
}

/// Stress test for spawn_external race condition between set_waiting() and signal().
///
/// This test spawns many short-lived tasks across multiple reactors to maximize
/// the chance of hitting the race window where signal() and set_waiting() execute
/// concurrently on different threads.
///
#[test]
fn test_spawn_external_race_stress() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let completed = Arc::new(AtomicUsize::new(0));
    let total_tasks = 1000;

    // Spawn tasks from external thread to different reactors.
    //
    let handles: Vec<_> = (0..total_tasks)
        .map(|i| {
            let completed = completed.clone();
            let reactor_id = i % 4;

            Coordinator::reactor(reactor_id).spawn_external_with_result(async move {
                // Minimal work - just increment counter
                completed.fetch_add(1, Ordering::Relaxed);
            })
        })
        .collect();

    coordinator.join_all().unwrap();

    // Verify all tasks completed.
    //
    for handle in handles {
        handle.unwrap_result();
    }

    assert_eq!(total_tasks, completed.load(Ordering::Relaxed));
}

/// Extreme stress test: reactors chain-call other reactors.
///
/// Each task spawns a continuation on a DIFFERENT reactor, creating deep
/// cross-reactor chains. This maximizes concurrent signal()/set_waiting()
/// pairs across different threads.
///
/// Pattern: reactor 0 -> reactor 1 -> reactor 2 -> reactor 3 -> reactor 0 -> ...
///
fn chain_step(
    current_reactor: usize,
    remaining: usize,
    completed: Arc<std::sync::atomic::AtomicUsize>,
    done_event: Arc<starfish_core::preemptive_synchronization::countdown_event::CountdownEvent>,
) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
    use std::sync::atomic::Ordering;

    Box::pin(async move {
        if remaining == 0 {
            completed.fetch_add(1, Ordering::Relaxed);
            done_event.signal();
            return;
        }

        // Spawn continuation on the NEXT reactor (round-robin).
        //
        let next_reactor = (current_reactor + 1) % 4;
        let handle = Coordinator::instance()
            .reactor_instance(next_reactor)
            .spawn_external_with_result(chain_step(
                next_reactor,
                remaining - 1,
                completed,
                done_event,
            ));

        // Wait for the continuation to complete (need Box::pin for recursive future).
        //
        Box::pin(handle).await;
    })
}

#[test]
fn test_spawn_external_chain_stress() {
    use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut coordinator = Coordinator::new();
    coordinator.initialize(4);

    let completed = Arc::new(AtomicUsize::new(0));
    let chain_depth = 10; // Each chain goes 10 reactors deep
    let num_chains = 100; // Start 100 parallel chains

    // Use CountdownEvent to wait for all chains to complete.
    //
    let done_event = Arc::new(CountdownEvent::new(num_chains));

    // Start chains from different reactors.
    //
    for i in 0..num_chains {
        let start_reactor = i % 4;
        let completed = completed.clone();
        let done_event = done_event.clone();

        let _ = Coordinator::reactor(start_reactor).spawn_external(chain_step(
            start_reactor,
            chain_depth,
            completed,
            done_event,
        ));
    }

    // Wait for all chains to complete before shutdown.
    //
    done_event.wait();

    coordinator.join_all().unwrap();

    assert_eq!(num_chains, completed.load(Ordering::Relaxed));
}
