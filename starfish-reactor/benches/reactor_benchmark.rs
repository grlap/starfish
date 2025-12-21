use std::future::Future;

use criterion::{Criterion, black_box, criterion_group, criterion_main};

use starfish_reactor::cooperative_io::io_manager::NoopIOManagerCreateOptions;
use starfish_reactor::coordinator::Coordinator;
use starfish_reactor::reactor::Reactor;

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn fibonacci(n: u64) -> u64 {
    match n {
        0 => 1,
        1 => 1,
        n => fibonacci(n - 1) + fibonacci(n - 2),
    }
}

async fn fibonacci_spawn_local(n: u64) -> u64 {
    let local_reactor = Reactor::local_instance();

    match n {
        0 => 1,
        1 => 1,
        n => {
            let result_future_1 = local_reactor.spawn_with_result(fibonacci_spawn_local(n - 1));
            let result_future_2 = local_reactor.spawn_with_result(fibonacci_spawn_local(n - 2));

            let n_1 = result_future_1.await;
            let n_2 = result_future_2.await;
            n_1 + n_2
        }
    }
}

#[allow(clippy::manual_async_fn)]
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

                let n_1 = result_future_1.await;
                let n_2 = result_future_2.await;
                n_1 + n_2
            }
        }
    }
}

#[allow(clippy::manual_async_fn)]
fn fibonacci_spawn_external_box_pin(n: u64) -> impl Future<Output = u64> + Send {
    async move {
        match n {
            0 => 1,
            1 => 1,
            n => {
                let result_future_1 = Coordinator::instance()
                    .reactor_instance(0)
                    .spawn_external_with_result(fibonacci_spawn_external_box_pin(n - 1));
                let result_future_2 = Coordinator::instance()
                    .reactor_instance(0)
                    .spawn_external_with_result(fibonacci_spawn_external_box_pin(n - 2));

                let n_1 = Box::pin(result_future_1).await;
                let n_2 = Box::pin(result_future_2).await;
                n_1 + n_2
            }
        }
    }
}

fn fibonacci_spawn_local_benchmark(n: u64) {
    let mut reactor = Reactor::new();

    _ = reactor.spawn_with_result(fibonacci_spawn_local(n));

    reactor.run();
}

fn fibonacci_spawn_external_benchmark(n: u64) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 1);

    let reactor = coordinator.reactor_instance(0);

    _ = reactor.spawn_external(async move {
        let _: u64 = fibonacci_spawn_external(n).await;
    });

    coordinator.join_all().unwrap();
}

fn fibonacci_spawn_external_benchmark_box_pin(n: u64) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 1);

    let reactor = coordinator.reactor_instance(0);

    _ = reactor.spawn_external(async move {
        let _: u64 = fibonacci_spawn_external_box_pin(n).await;
    });

    coordinator.join_all().unwrap();
}

fn criterion_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("reactor_spawn");

    group.bench_function("call", |b| b.iter(|| fibonacci(black_box(18))));

    group.bench_function("spawn_local", |b| {
        b.iter(|| fibonacci_spawn_local_benchmark(black_box(18)))
    });

    group.bench_function("spawn_external", |b| {
        b.iter(|| fibonacci_spawn_external_benchmark(black_box(18)))
    });

    group.bench_function("spawn_external_box_pin", |b| {
        b.iter(|| fibonacci_spawn_external_benchmark_box_pin(black_box(18)))
    });

    group.finish();
}

/// Benchmark for cross-reactor message passing latency.
///
/// Measures the round-trip time for spawn_external calls between reactors.
/// This is the core metric for comparing with Seastar's submit_to().
///
fn cross_reactor_benchmark(c: &mut Criterion) {
    use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut group = c.benchmark_group("cross_reactor");

    // Baseline: regular function call overhead.
    //
    group.bench_function("baseline_function_call", |b| {
        b.iter(|| {
            let mut sum = 0u64;
            for i in 0..1000 {
                sum = sum.wrapping_add(black_box(i));
            }
            black_box(sum)
        })
    });

    // Baseline: atomic increment (memory synchronization overhead).
    //
    group.bench_function("baseline_atomic_increment", |b| {
        let counter = AtomicUsize::new(0);
        b.iter(|| {
            for _ in 0..1000 {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            black_box(counter.load(Ordering::Relaxed))
        })
    });

    // Cross-reactor: spawn_external to same reactor (measures queue overhead).
    //
    group.bench_function("spawn_external_same_reactor_1000", |b| {
        b.iter(|| {
            let mut coordinator = Coordinator::new();
            coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 1);

            let completed = Arc::new(AtomicUsize::new(0));
            let done_event = Arc::new(CountdownEvent::new(1000));

            for _ in 0..1000 {
                let completed = completed.clone();
                let done_event = done_event.clone();

                drop(Coordinator::reactor(0).spawn_external(async move {
                    completed.fetch_add(1, Ordering::Relaxed);
                    done_event.signal();
                }));
            }

            done_event.wait();
            coordinator.join_all().unwrap();

            black_box(completed.load(Ordering::Relaxed))
        })
    });

    // Cross-reactor: spawn_external to different reactors (round-robin).
    //
    group.bench_function("spawn_external_cross_reactor_1000", |b| {
        b.iter(|| {
            let mut coordinator = Coordinator::new();
            coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 4);

            let completed = Arc::new(AtomicUsize::new(0));
            let done_event = Arc::new(CountdownEvent::new(1000));

            for i in 0..1000 {
                let completed = completed.clone();
                let done_event = done_event.clone();
                let reactor_id = i % 4;

                drop(Coordinator::reactor(reactor_id).spawn_external(async move {
                    completed.fetch_add(1, Ordering::Relaxed);
                    done_event.signal();
                }));
            }

            done_event.wait();
            coordinator.join_all().unwrap();

            black_box(completed.load(Ordering::Relaxed))
        })
    });

    // Cross-reactor ping-pong: reactor 0 -> reactor 1 -> reactor 0 (round-trip).
    //
    group.bench_function("cross_reactor_pingpong_100", |b| {
        b.iter(|| {
            let mut coordinator = Coordinator::new();
            coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 2);

            let done_event = Arc::new(CountdownEvent::new(1));

            fn pingpong(
                remaining: usize,
                done_event: Arc<CountdownEvent>,
            ) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
                Box::pin(async move {
                    if remaining == 0 {
                        done_event.signal();
                        return;
                    }

                    // Ping to the other reactor.
                    //
                    let current = if remaining.is_multiple_of(2) { 0 } else { 1 };
                    let next = 1 - current;

                    let handle = Coordinator::instance()
                        .reactor_instance(next)
                        .spawn_external_with_result(pingpong(remaining - 1, done_event));

                    Box::pin(handle).await;
                })
            }

            drop(Coordinator::reactor(0).spawn_external(pingpong(100, done_event.clone())));

            done_event.wait();
            coordinator.join_all().unwrap();
        })
    });

    // Chain stress: reactor 0 -> 1 -> 2 -> 3 -> 0 -> ... (measures chain overhead).
    //
    group.bench_function("cross_reactor_chain_depth_10_chains_100", |b| {
        b.iter(|| {
            let mut coordinator = Coordinator::new();
            coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 4);

            let completed = Arc::new(AtomicUsize::new(0));
            let done_event = Arc::new(CountdownEvent::new(100));

            fn chain_step(
                current_reactor: usize,
                remaining: usize,
                completed: Arc<AtomicUsize>,
                done_event: Arc<CountdownEvent>,
            ) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
                Box::pin(async move {
                    if remaining == 0 {
                        completed.fetch_add(1, Ordering::Relaxed);
                        done_event.signal();
                        return;
                    }

                    let next_reactor = (current_reactor + 1) % 4;
                    let handle = Coordinator::instance()
                        .reactor_instance(next_reactor)
                        .spawn_external_with_result(chain_step(
                            next_reactor,
                            remaining - 1,
                            completed,
                            done_event,
                        ));

                    Box::pin(handle).await;
                })
            }

            for i in 0..100 {
                let start_reactor = i % 4;
                let completed = completed.clone();
                let done_event = done_event.clone();

                let result_future = Coordinator::reactor(start_reactor).spawn_external(chain_step(
                    start_reactor,
                    10,
                    completed,
                    done_event,
                ));

                drop(result_future);
            }

            done_event.wait();
            coordinator.join_all().unwrap();

            black_box(completed.load(Ordering::Relaxed))
        })
    });

    group.finish();
}

/// Benchmark to isolate individual components of cross-reactor message passing.
///
/// Measures:
/// - SegQueue push/pop overhead
/// - Box::pin allocation
/// - ArcPointer allocation (WaitOneSharedState)
/// - Full spawn_external overhead
///
fn component_benchmark(c: &mut Criterion) {
    use crossbeam::queue::SegQueue;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut group = c.benchmark_group("components");

    // Baseline: SegQueue push/pop (lock-free MPMC queue).
    //
    group.bench_function("segqueue_push_pop_1000", |b| {
        let queue: SegQueue<usize> = SegQueue::new();
        b.iter(|| {
            for i in 0..1000 {
                queue.push(i);
            }
            for _ in 0..1000 {
                black_box(queue.pop());
            }
        })
    });

    // Baseline: Box allocation (simulates future boxing).
    //
    group.bench_function("box_alloc_1000", |b| {
        b.iter(|| {
            for i in 0..1000 {
                let boxed: Box<usize> = Box::new(i);
                black_box(boxed);
            }
        })
    });

    // Baseline: Box::pin allocation (what spawn_external does).
    //
    group.bench_function("box_pin_future_1000", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                let future = async { 42 };
                let pinned: Pin<Box<dyn Future<Output = i32> + Send>> = Box::pin(future);
                // We're measuring allocation overhead, not execution.
                drop(black_box(pinned));
            }
        })
    });

    // Baseline: Arc allocation (simulates ArcPointer for WaitOneSharedState).
    //
    group.bench_function("arc_alloc_1000", |b| {
        b.iter(|| {
            for i in 0..1000 {
                let arc = Arc::new(i);
                black_box(arc);
            }
        })
    });

    // Baseline: Arc clone (what happens when passing Arc between threads).
    //
    group.bench_function("arc_clone_1000", |b| {
        let arc = Arc::new(42usize);
        b.iter(|| {
            for _ in 0..1000 {
                let cloned = arc.clone();
                black_box(cloned);
            }
        })
    });

    // Baseline: AtomicBool swap (core of signal/set_waiting synchronization).
    //
    group.bench_function("atomic_swap_1000", |b| {
        use std::sync::atomic::AtomicBool;
        let flag = AtomicBool::new(false);
        b.iter(|| {
            for _ in 0..1000 {
                black_box(flag.swap(true, Ordering::AcqRel));
                flag.store(false, Ordering::Release);
            }
        })
    });

    // Combined: SegQueue + Box::pin (queue overhead with allocation).
    //
    group.bench_function("segqueue_boxpin_1000", |b| {
        let queue: SegQueue<Pin<Box<dyn Future<Output = i32> + Send>>> = SegQueue::new();
        b.iter(|| {
            for _ in 0..1000 {
                let future = async { 42 };
                queue.push(Box::pin(future));
            }
            for _ in 0..1000 {
                black_box(queue.pop());
            }
        })
    });

    // Combined: Arc + AtomicBool (WaitOneSharedState simulation).
    //
    group.bench_function("arc_atomic_swap_1000", |b| {
        use std::sync::atomic::AtomicBool;

        struct SimulatedState {
            is_signaled: AtomicBool,
        }

        b.iter(|| {
            for _ in 0..1000 {
                let state = Arc::new(SimulatedState {
                    is_signaled: AtomicBool::new(false),
                });
                let was_signaled = state.is_signaled.swap(true, Ordering::AcqRel);
                black_box(was_signaled);
            }
        })
    });

    // Contended: SegQueue with multiple threads pushing.
    //
    group.bench_function("segqueue_contended_1000", |b| {
        use std::thread;

        b.iter(|| {
            let queue: Arc<SegQueue<usize>> = Arc::new(SegQueue::new());
            let counter = Arc::new(AtomicUsize::new(0));

            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let queue = queue.clone();
                    let counter = counter.clone();
                    thread::spawn(move || {
                        for i in 0..250 {
                            queue.push(i);
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }

            // Drain the queue
            while queue.pop().is_some() {}

            black_box(counter.load(Ordering::Relaxed))
        })
    });

    group.finish();
}

/// Benchmark to isolate spawn_external overhead vs raw queue operations.
///
/// Tests the full spawn_external path with a pre-initialized coordinator
/// to avoid coordinator setup overhead in each iteration.
///
fn spawn_external_detailed_benchmark(c: &mut Criterion) {
    use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut group = c.benchmark_group("spawn_external_detailed");

    // Pre-create coordinator once, reuse across iterations.
    // This isolates spawn_external overhead from coordinator creation.
    //
    group.bench_function("spawn_external_no_coordinator_setup_1000", |b| {
        let mut coordinator = Coordinator::new();
        coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 1);

        b.iter(|| {
            let completed = Arc::new(AtomicUsize::new(0));
            let done_event = Arc::new(CountdownEvent::new(1000));

            // Collect futures to keep them alive until completion.
            //
            let _futures: Vec<_> = (0..1000)
                .map(|_| {
                    let completed = completed.clone();
                    let done_event = done_event.clone();

                    Coordinator::reactor(0).spawn_external(async move {
                        completed.fetch_add(1, Ordering::Relaxed);
                        done_event.signal();
                    })
                })
                .collect();

            done_event.wait();
            black_box(completed.load(Ordering::Relaxed))
        });

        coordinator.join_all().unwrap();
    });

    // Measure just the future creation + queue push (no execution).
    // Uses a custom minimal future to isolate overhead.
    //
    group.bench_function("spawn_external_enqueue_only_1000", |b| {
        use crossbeam::queue::SegQueue;
        use std::pin::Pin;

        // Simulate FutureRuntime structure.
        //
        struct FakeRuntime {
            _future: Pin<Box<dyn Future<Output = ()> + Send>>,
            _signaler: (), // Placeholder for CooperativeWaitOneSignaler
        }

        let queue: SegQueue<FakeRuntime> = SegQueue::new();

        b.iter(|| {
            for _ in 0..1000 {
                let future = async {};
                let runtime = FakeRuntime {
                    _future: Box::pin(future),
                    _signaler: (),
                };
                queue.push(runtime);
            }

            // Drain without processing.
            //
            while queue.pop().is_some() {}
        })
    });

    // Measure Arc<WaitOneSharedState> creation overhead.
    //
    group.bench_function("wait_one_state_alloc_1000", |b| {
        use std::cell::UnsafeCell;
        use std::sync::atomic::AtomicBool;

        // Simulate WaitOneSharedState.
        //
        struct FakeWaitOneState {
            _reactor_ptr: *mut (),
            _waiting_future: UnsafeCell<Option<()>>,
            _is_signaled: AtomicBool,
        }

        // SAFETY: This is just for benchmarking, not actual use.
        unsafe impl Send for FakeWaitOneState {}
        unsafe impl Sync for FakeWaitOneState {}

        // Simulate ArcPointer (which is Arc internally).
        //
        b.iter(|| {
            for _ in 0..1000 {
                let state = Arc::new(FakeWaitOneState {
                    _reactor_ptr: std::ptr::null_mut(),
                    _waiting_future: UnsafeCell::new(None),
                    _is_signaled: AtomicBool::new(false),
                });
                // Simulate creating both future and signaler (2 Arc clones).
                //
                let _future_arc = state.clone();
                let _signaler_arc = state;
                black_box((_future_arc, _signaler_arc));
            }
        })
    });

    // Measure FutureRuntime creation with real-ish structure.
    //
    group.bench_function("future_runtime_create_1000", |b| {
        use std::pin::Pin;
        use std::sync::atomic::AtomicBool;

        struct FakeSignaler {
            _state: Arc<AtomicBool>,
        }

        struct FakeRuntime {
            _future: Pin<Box<dyn Future<Output = ()> + Send>>,
            _signaler: FakeSignaler,
            _execution_count: u32,
            _is_first_yield: bool,
        }

        b.iter(|| {
            for _ in 0..1000 {
                let state = Arc::new(AtomicBool::new(false));
                let runtime = FakeRuntime {
                    _future: Box::pin(async {}),
                    _signaler: FakeSignaler {
                        _state: state.clone(),
                    },
                    _execution_count: 0,
                    _is_first_yield: true,
                };
                black_box(runtime);
            }
        })
    });

    // Measure full WaitOne handshake (signal + set_waiting simulation).
    //
    group.bench_function("wait_one_handshake_1000", |b| {
        use std::sync::atomic::AtomicBool;

        struct SimulatedWaitOne {
            is_signaled: AtomicBool,
        }

        b.iter(|| {
            for _ in 0..1000 {
                let state = Arc::new(SimulatedWaitOne {
                    is_signaled: AtomicBool::new(false),
                });

                // Simulate signal() side.
                //
                let state_for_signal = state.clone();
                let was_signaled_signal = state_for_signal.is_signaled.swap(true, Ordering::AcqRel);

                // Simulate set_waiting() side (if signal didn't run first).
                //
                if !was_signaled_signal {
                    let was_signaled_wait = state.is_signaled.swap(true, Ordering::AcqRel);
                    black_box(was_signaled_wait);
                }
            }
        })
    });

    // Measure Coordinator::reactor() lookup overhead.
    //
    group.bench_function("coordinator_reactor_lookup_1000", |b| {
        let mut coordinator = Coordinator::new();
        coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 4);

        b.iter(|| {
            for i in 0..1000 {
                let reactor_id = i % 4;
                let _reactor = Coordinator::reactor(reactor_id);
                black_box(_reactor);
            }
        });

        coordinator.join_all().unwrap();
    });

    // Measure poll + context creation overhead.
    //
    group.bench_function("poll_context_create_1000", |b| {
        use std::task::{Context, RawWaker, RawWakerVTable, Waker};

        fn dummy_waker() -> Waker {
            static VTABLE: RawWakerVTable = RawWakerVTable::new(
                |_| RawWaker::new(std::ptr::null(), &VTABLE),
                |_| {},
                |_| {},
                |_| {},
            );
            unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
        }

        b.iter(|| {
            for _ in 0..1000 {
                let waker = dummy_waker();
                let mut context = Context::from_waker(&waker);
                black_box(&mut context);
            }
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    criterion_benchmark,
    cross_reactor_benchmark,
    component_benchmark,
    spawn_external_detailed_benchmark
);
criterion_main!(benches);
