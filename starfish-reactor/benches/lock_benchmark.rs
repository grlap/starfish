use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::Criterion;
use criterion::black_box;
use criterion::criterion_group;
use criterion::criterion_main;
use mimalloc::MiMalloc;
use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;
use starfish_reactor::cooperative_io::io_manager::NoopIOManagerCreateOptions;
use starfish_reactor::cooperative_synchronization::lock::CooperativeFairLock;
use starfish_reactor::cooperative_synchronization::lock::CooperativeReactorAwareLock;
use starfish_reactor::cooperative_synchronization::lock::CooperativeUnfairLock;
use starfish_reactor::coordinator::Coordinator;
use starfish_reactor::reactor::Reactor;
use starfish_reactor::reactor::cooperative_yield;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// =============================================================================
// Single Reactor Benchmarks (No Cross-Reactor Overhead)
// =============================================================================

fn single_reactor_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("lock_single_reactor");

    // FairLock: Single task, no contention.
    group.bench_function("fair_lock_no_contention_1000", |b| {
        b.iter(|| {
            let mut reactor = Reactor::new();
            let lock = CooperativeFairLock::new(0usize);

            let lock_clone = lock.clone();
            _ = reactor.spawn(async move {
                for _ in 0..1000 {
                    let mut guard = lock_clone.acquire().await;
                    *guard += 1;
                }
            });

            reactor.run();
            black_box(lock.try_acquire().map(|g| *g))
        })
    });

    // UnfairLock: Single task, no contention.
    group.bench_function("unfair_lock_no_contention_1000", |b| {
        b.iter(|| {
            let mut reactor = Reactor::new();
            let lock = CooperativeUnfairLock::new(0usize);

            let lock_clone = lock.clone();
            _ = reactor.spawn(async move {
                for _ in 0..1000 {
                    let mut guard = lock_clone.acquire().await;
                    *guard += 1;
                }
            });

            reactor.run();
            black_box(lock.try_acquire().map(|g| *g))
        })
    });

    // ReactorAwareLock: Single task, no contention.
    group.bench_function("reactor_aware_lock_no_contention_1000", |b| {
        b.iter(|| {
            let mut reactor = Reactor::new();
            let lock = CooperativeReactorAwareLock::new(0usize);

            let lock_clone = lock.clone();
            _ = reactor.spawn(async move {
                for _ in 0..1000 {
                    let mut guard = lock_clone.acquire().await;
                    *guard += 1;
                }
            });

            reactor.run();
            black_box(lock.try_acquire().map(|g| *g))
        })
    });

    // FairLock: Two tasks contending.
    group.bench_function("fair_lock_2_tasks_contention_1000", |b| {
        b.iter(|| {
            let mut reactor = Reactor::new();
            let lock = CooperativeFairLock::new(0usize);

            for _ in 0..2 {
                let lock = lock.clone();
                _ = reactor.spawn(async move {
                    for _ in 0..500 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                        drop(guard);
                        cooperative_yield().await;
                    }
                });
            }

            reactor.run();
            black_box(lock.try_acquire().map(|g| *g))
        })
    });

    // UnfairLock: Two tasks contending.
    group.bench_function("unfair_lock_2_tasks_contention_1000", |b| {
        b.iter(|| {
            let mut reactor = Reactor::new();
            let lock = CooperativeUnfairLock::new(0usize);

            for _ in 0..2 {
                let lock = lock.clone();
                _ = reactor.spawn(async move {
                    for _ in 0..500 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                        drop(guard);
                        cooperative_yield().await;
                    }
                });
            }

            reactor.run();
            black_box(lock.try_acquire().map(|g| *g))
        })
    });

    // ReactorAwareLock: Two tasks contending.
    group.bench_function("reactor_aware_lock_2_tasks_contention_1000", |b| {
        b.iter(|| {
            let mut reactor = Reactor::new();
            let lock = CooperativeReactorAwareLock::new(0usize);

            for _ in 0..2 {
                let lock = lock.clone();
                _ = reactor.spawn(async move {
                    for _ in 0..500 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                        drop(guard);
                        cooperative_yield().await;
                    }
                });
            }

            reactor.run();
            black_box(lock.try_acquire().map(|g| *g))
        })
    });

    // FairLock: Many tasks (high contention).
    group.bench_function("fair_lock_10_tasks_contention_1000", |b| {
        b.iter(|| {
            let mut reactor = Reactor::new();
            let lock = CooperativeFairLock::new(0usize);

            for _ in 0..10 {
                let lock = lock.clone();
                _ = reactor.spawn(async move {
                    for _ in 0..100 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                        drop(guard);
                        cooperative_yield().await;
                    }
                });
            }

            reactor.run();
            black_box(lock.try_acquire().map(|g| *g))
        })
    });

    // UnfairLock: Many tasks (high contention).
    group.bench_function("unfair_lock_10_tasks_contention_1000", |b| {
        b.iter(|| {
            let mut reactor = Reactor::new();
            let lock = CooperativeUnfairLock::new(0usize);

            for _ in 0..10 {
                let lock = lock.clone();
                _ = reactor.spawn(async move {
                    for _ in 0..100 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                        drop(guard);
                        cooperative_yield().await;
                    }
                });
            }

            reactor.run();
            black_box(lock.try_acquire().map(|g| *g))
        })
    });

    // ReactorAwareLock: Many tasks (high contention).
    group.bench_function("reactor_aware_lock_10_tasks_contention_1000", |b| {
        b.iter(|| {
            let mut reactor = Reactor::new();
            let lock = CooperativeReactorAwareLock::new(0usize);

            for _ in 0..10 {
                let lock = lock.clone();
                _ = reactor.spawn(async move {
                    for _ in 0..100 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                        drop(guard);
                        cooperative_yield().await;
                    }
                });
            }

            reactor.run();
            black_box(lock.try_acquire().map(|g| *g))
        })
    });

    group.finish();
}

// =============================================================================
// Cross-Reactor Benchmarks (4 reactors)
// =============================================================================

fn cross_reactor_4_benchmark(c: &mut Criterion) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 4);

    let mut group = c.benchmark_group("lock_cross_reactor_4");
    // Reduce sample size for cross-reactor tests which are slower.
    group.sample_size(20);
    group.warm_up_time(std::time::Duration::from_millis(500));

    // FairLock: 4 reactors, 1 task each.
    let fair_lock = CooperativeFairLock::new(0usize);
    group.bench_function("fair_lock_1000", |b| {
        b.iter(|| {
            let done_event = Arc::new(CountdownEvent::new(4));

            for reactor_id in 0..4 {
                let lock = fair_lock.clone();
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
            black_box(fair_lock.try_acquire().map(|g| *g))
        });
    });

    // UnfairLock: 4 reactors, 1 task each.
    let unfair_lock = CooperativeUnfairLock::new(0usize);
    group.bench_function("unfair_lock_1000", |b| {
        b.iter(|| {
            let done_event = Arc::new(CountdownEvent::new(4));

            for reactor_id in 0..4 {
                let lock = unfair_lock.clone();
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
            black_box(unfair_lock.try_acquire().map(|g| *g))
        });
    });

    // ReactorAwareLock: 4 reactors, 1 task each.
    let reactor_aware_lock = CooperativeReactorAwareLock::new(0usize);
    group.bench_function("reactor_aware_lock_1000", |b| {
        b.iter(|| {
            let done_event = Arc::new(CountdownEvent::new(4));

            for reactor_id in 0..4 {
                let lock = reactor_aware_lock.clone();
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
            black_box(reactor_aware_lock.try_acquire().map(|g| *g))
        });
    });

    group.finish();
    coordinator.join_all().unwrap();
}

// =============================================================================
// Ping-Pong Benchmarks (Lock Transfer Between Two Reactors)
// =============================================================================

fn pingpong_benchmark(c: &mut Criterion) {
    let mut coordinator = Coordinator::new();
    coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 2);

    let mut group = c.benchmark_group("lock_pingpong");
    // Reduce sample size for cross-reactor tests which are slower.
    group.sample_size(20);
    group.warm_up_time(std::time::Duration::from_millis(500));

    // FairLock: Two reactors rapidly passing lock back and forth.
    let fair_lock = CooperativeFairLock::new(0usize);
    group.bench_function("fair_lock_1000", |b| {
        b.iter(|| {
            let done_event = Arc::new(CountdownEvent::new(2));

            for reactor_id in 0..2 {
                let lock = fair_lock.clone();
                let done = done_event.clone();

                drop(Coordinator::reactor(reactor_id).spawn_external(async move {
                    for _ in 0..500 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                    }
                    done.signal();
                }));
            }

            done_event.wait();
            black_box(fair_lock.try_acquire().map(|g| *g))
        });
    });

    // UnfairLock: Two reactors rapidly passing lock back and forth.
    let unfair_lock = CooperativeUnfairLock::new(0usize);
    group.bench_function("unfair_lock_1000", |b| {
        b.iter(|| {
            let done_event = Arc::new(CountdownEvent::new(2));

            for reactor_id in 0..2 {
                let lock = unfair_lock.clone();
                let done = done_event.clone();

                drop(Coordinator::reactor(reactor_id).spawn_external(async move {
                    for _ in 0..500 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                    }
                    done.signal();
                }));
            }

            done_event.wait();
            black_box(unfair_lock.try_acquire().map(|g| *g))
        });
    });

    // ReactorAwareLock: Two reactors rapidly passing lock back and forth.
    let reactor_aware_lock = CooperativeReactorAwareLock::new(0usize);
    group.bench_function("reactor_aware_lock_1000", |b| {
        b.iter(|| {
            let done_event = Arc::new(CountdownEvent::new(2));

            for reactor_id in 0..2 {
                let lock = reactor_aware_lock.clone();
                let done = done_event.clone();

                drop(Coordinator::reactor(reactor_id).spawn_external(async move {
                    for _ in 0..500 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                    }
                    done.signal();
                }));
            }

            done_event.wait();
            black_box(reactor_aware_lock.try_acquire().map(|g| *g))
        });
    });

    group.finish();
    coordinator.join_all().unwrap();
}

// =============================================================================
// try_acquire Benchmarks (Synchronous Path)
// =============================================================================

fn try_acquire_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("lock_try_acquire");

    // FairLock: try_acquire when lock is free.
    group.bench_function("fair_lock_try_acquire_success_10000", |b| {
        let lock = CooperativeFairLock::new(0usize);

        b.iter(|| {
            for _ in 0..10000 {
                let guard = lock.try_acquire();
                black_box(guard);
            }
        })
    });

    // UnfairLock: try_acquire when lock is free.
    group.bench_function("unfair_lock_try_acquire_success_10000", |b| {
        let lock = CooperativeUnfairLock::new(0usize);

        b.iter(|| {
            for _ in 0..10000 {
                let guard = lock.try_acquire();
                black_box(guard);
            }
        })
    });

    // ReactorAwareLock: try_acquire when lock is free.
    group.bench_function("reactor_aware_lock_try_acquire_success_10000", |b| {
        let lock = CooperativeReactorAwareLock::new(0usize);

        b.iter(|| {
            for _ in 0..10000 {
                let guard = lock.try_acquire();
                black_box(guard);
            }
        })
    });

    // FairLock: try_acquire when lock is held (failure path).
    group.bench_function("fair_lock_try_acquire_failure_10000", |b| {
        let lock = CooperativeFairLock::new(0usize);
        let _guard = lock.try_acquire().unwrap();

        b.iter(|| {
            for _ in 0..10000 {
                let result = lock.try_acquire();
                black_box(result);
            }
        })
    });

    // UnfairLock: try_acquire when lock is held (failure path).
    group.bench_function("unfair_lock_try_acquire_failure_10000", |b| {
        let lock = CooperativeUnfairLock::new(0usize);
        let _guard = lock.try_acquire().unwrap();

        b.iter(|| {
            for _ in 0..10000 {
                let result = lock.try_acquire();
                black_box(result);
            }
        })
    });

    // ReactorAwareLock: try_acquire when lock is held (failure path).
    group.bench_function("reactor_aware_lock_try_acquire_failure_10000", |b| {
        let lock = CooperativeReactorAwareLock::new(0usize);
        let _guard = lock.try_acquire().unwrap();

        b.iter(|| {
            for _ in 0..10000 {
                let result = lock.try_acquire();
                black_box(result);
            }
        })
    });

    group.finish();
}

// =============================================================================
// Comparison with std::sync::Mutex (Baseline)
// =============================================================================

fn baseline_benchmark(c: &mut Criterion) {
    use std::sync::Mutex;

    let mut group = c.benchmark_group("lock_baseline");

    // std::sync::Mutex: Single-threaded, no contention.
    group.bench_function("std_mutex_no_contention_10000", |b| {
        let mutex = Mutex::new(0usize);

        b.iter(|| {
            for _ in 0..10000 {
                let mut guard = mutex.lock().unwrap();
                *guard += 1;
            }
            black_box(*mutex.lock().unwrap())
        })
    });

    // std::sync::Mutex: Inside async reactor context (fair comparison with cooperative locks).
    group.bench_function("std_mutex_in_reactor_no_contention_1000", |b| {
        b.iter(|| {
            let mut reactor = Reactor::new();
            let mutex = Arc::new(Mutex::new(0usize));

            let mutex_clone = mutex.clone();
            _ = reactor.spawn(async move {
                for _ in 0..1000 {
                    let mut guard = mutex_clone.lock().unwrap();
                    *guard += 1;
                }
            });

            reactor.run();
            black_box(*mutex.lock().unwrap())
        })
    });

    // std::sync::Mutex: Multi-threaded contention.
    group.bench_function("std_mutex_4_threads_contention_10000", |b| {
        use std::thread;

        b.iter(|| {
            let mutex = Arc::new(Mutex::new(0usize));
            let counter = Arc::new(AtomicUsize::new(0));

            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let mutex = mutex.clone();
                    let counter = counter.clone();
                    thread::spawn(move || {
                        for _ in 0..2500 {
                            let mut guard = mutex.lock().unwrap();
                            *guard += 1;
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }

            black_box(*mutex.lock().unwrap())
        })
    });

    group.finish();
}

// =============================================================================
// Throughput Benchmarks
// =============================================================================

fn throughput_benchmark(c: &mut Criterion) {
    use criterion::Throughput;

    let mut coordinator = Coordinator::new();
    coordinator.initialize_with_io_manager(NoopIOManagerCreateOptions::new(), 4);

    let mut group = c.benchmark_group("lock_throughput");
    group.throughput(Throughput::Elements(10000));
    // Reduce sample size for cross-reactor tests which are slower.
    group.sample_size(20);
    group.warm_up_time(std::time::Duration::from_millis(500));

    // FairLock throughput: operations per second.
    let fair_lock = CooperativeFairLock::new(0usize);
    group.bench_function("fair_lock_ops_per_sec", |b| {
        b.iter(|| {
            let done_event = Arc::new(CountdownEvent::new(4));

            for reactor_id in 0..4 {
                let lock = fair_lock.clone();
                let done = done_event.clone();

                drop(Coordinator::reactor(reactor_id).spawn_external(async move {
                    for _ in 0..2500 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                    }
                    done.signal();
                }));
            }

            done_event.wait();
            black_box(fair_lock.try_acquire().map(|g| *g))
        });
    });

    // UnfairLock throughput.
    let unfair_lock = CooperativeUnfairLock::new(0usize);
    group.bench_function("unfair_lock_ops_per_sec", |b| {
        b.iter(|| {
            let done_event = Arc::new(CountdownEvent::new(4));

            for reactor_id in 0..4 {
                let lock = unfair_lock.clone();
                let done = done_event.clone();

                drop(Coordinator::reactor(reactor_id).spawn_external(async move {
                    for _ in 0..2500 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                    }
                    done.signal();
                }));
            }

            done_event.wait();
            black_box(unfair_lock.try_acquire().map(|g| *g))
        });
    });

    // ReactorAwareLock throughput.
    let reactor_aware_lock = CooperativeReactorAwareLock::new(0usize);
    group.bench_function("reactor_aware_lock_ops_per_sec", |b| {
        b.iter(|| {
            let done_event = Arc::new(CountdownEvent::new(4));

            for reactor_id in 0..4 {
                let lock = reactor_aware_lock.clone();
                let done = done_event.clone();

                drop(Coordinator::reactor(reactor_id).spawn_external(async move {
                    for _ in 0..2500 {
                        let mut guard = lock.acquire().await;
                        *guard += 1;
                    }
                    done.signal();
                }));
            }

            done_event.wait();
            black_box(reactor_aware_lock.try_acquire().map(|g| *g))
        });
    });

    group.finish();
    coordinator.join_all().unwrap();
}

criterion_group!(
    benches,
    single_reactor_benchmark,
    cross_reactor_4_benchmark,
    pingpong_benchmark,
    try_acquire_benchmark,
    baseline_benchmark,
    throughput_benchmark,
);
criterion_main!(benches);
