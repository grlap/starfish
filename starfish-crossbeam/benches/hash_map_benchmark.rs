//! Benchmark for SplitOrderedHashMap with epoch-based memory reclamation.
//!
//! Run with: cargo bench --package starfish-crossbeam --bench hash_map_benchmark

use criterion::BatchSize;
use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::black_box;
use criterion::criterion_group;
use criterion::criterion_main;
use mimalloc::MiMalloc;
use std::sync::Arc;
use std::thread;

use starfish_core::common_tests::bench_utils::thread_counts;
use starfish_core::data_structures::SplitOrderedHashMap;
use starfish_core::data_structures::hash::MapCollection;
use starfish_core::data_structures::map_entry::MapEntry;
use starfish_core::data_structures::sorted::skip_list::SkipList;
use starfish_crossbeam::EpochGuard;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// Type aliases for convenience
type EpochHashMap<K, V> = SplitOrderedHashMap<K, V, EpochGuard>;
type EpochSkipMap<K, V> = SkipList<MapEntry<K, V>, EpochGuard>;

// ============================================================================
// Concurrent insert benchmark
// ============================================================================

fn split_ordered_hash_table_insert(thread_count: usize, iteration_count: usize) {
    let table: Arc<EpochHashMap<usize, String>> = Arc::new(EpochHashMap::new());
    let mut handles = vec![];

    for i in 0..thread_count {
        let table_clone = Arc::clone(&table);
        let handle = thread::spawn(move || {
            for j in 0..iteration_count {
                table_clone.insert(
                    i * iteration_count + j,
                    format!("value_{}", i * iteration_count + j),
                );
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn skip_map_insert(thread_count: usize, iteration_count: usize) {
    let map: Arc<EpochSkipMap<usize, String>> = Arc::new(EpochSkipMap::new());
    let mut handles = vec![];

    for i in 0..thread_count {
        let m = Arc::clone(&map);
        let handle = thread::spawn(move || {
            for j in 0..iteration_count {
                m.insert(
                    i * iteration_count + j,
                    format!("value_{}", i * iteration_count + j),
                );
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

// ============================================================================
// Mixed operations benchmark (insert + get + remove)
// ============================================================================

fn setup_hash_map_mixed(
    thread_count: usize,
    iteration_count: usize,
) -> Arc<EpochHashMap<usize, String>> {
    let table: Arc<EpochHashMap<usize, String>> = Arc::new(EpochHashMap::new());
    for i in 0..(thread_count * iteration_count / 2) {
        table.insert(i, format!("value_{}", i));
    }
    table
}

fn run_hash_map_mixed(
    table: Arc<EpochHashMap<usize, String>>,
    thread_count: usize,
    iteration_count: usize,
) {
    let mut handles = vec![];

    for t in 0..thread_count {
        let table_clone = Arc::clone(&table);
        let handle = thread::spawn(move || {
            let base = t * iteration_count;
            for i in 0..iteration_count {
                match i % 3 {
                    0 => {
                        table_clone.insert(base + i + 1_000_000, format!("new_{}", base + i));
                    }
                    1 => {
                        black_box(table_clone.contains(&(i / 2)));
                    }
                    2 => {
                        table_clone.remove(&(i / 2));
                    }
                    _ => unreachable!(),
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn setup_skip_map_mixed(
    thread_count: usize,
    iteration_count: usize,
) -> Arc<EpochSkipMap<usize, String>> {
    let map: Arc<EpochSkipMap<usize, String>> = Arc::new(EpochSkipMap::new());
    for i in 0..(thread_count * iteration_count / 2) {
        map.insert(i, format!("value_{}", i));
    }
    map
}

fn run_skip_map_mixed(
    map: Arc<EpochSkipMap<usize, String>>,
    thread_count: usize,
    iteration_count: usize,
) {
    let mut handles = vec![];

    for t in 0..thread_count {
        let m = Arc::clone(&map);
        let handle = thread::spawn(move || {
            let base = t * iteration_count;
            for i in 0..iteration_count {
                match i % 3 {
                    0 => {
                        m.insert(base + i + 1_000_000, format!("new_{}", base + i));
                    }
                    1 => {
                        black_box(m.contains(&(i / 2)));
                    }
                    2 => {
                        m.remove(&(i / 2));
                    }
                    _ => unreachable!(),
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

// ============================================================================
// Concurrent update benchmark
// ============================================================================

fn setup_hash_map_update(
    thread_count: usize,
    iteration_count: usize,
) -> Arc<EpochHashMap<usize, String>> {
    let table: Arc<EpochHashMap<usize, String>> = Arc::new(EpochHashMap::new());
    for i in 0..(thread_count * iteration_count) {
        table.insert(i, format!("value_{}", i));
    }
    table
}

fn run_hash_map_update(
    table: Arc<EpochHashMap<usize, String>>,
    thread_count: usize,
    iteration_count: usize,
) {
    let mut handles = vec![];

    for t in 0..thread_count {
        let table_clone = Arc::clone(&table);
        let handle = thread::spawn(move || {
            for j in 0..iteration_count {
                let key = t * iteration_count + j;
                table_clone.update(key, format!("updated_{}", key));
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn setup_skip_map_update(
    thread_count: usize,
    iteration_count: usize,
) -> Arc<EpochSkipMap<usize, String>> {
    let map: Arc<EpochSkipMap<usize, String>> = Arc::new(EpochSkipMap::new());
    for i in 0..(thread_count * iteration_count) {
        map.insert(i, format!("value_{}", i));
    }
    map
}

fn run_skip_map_update(
    map: Arc<EpochSkipMap<usize, String>>,
    thread_count: usize,
    iteration_count: usize,
) {
    let mut handles = vec![];

    for t in 0..thread_count {
        let m = Arc::clone(&map);
        let handle = thread::spawn(move || {
            for j in 0..iteration_count {
                let key = t * iteration_count + j;
                m.update(key, format!("updated_{}", key));
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

// ============================================================================
// High contention benchmark
// ============================================================================

fn split_ordered_hash_table_contention(thread_count: usize, iteration_count: usize) {
    let table: Arc<EpochHashMap<usize, String>> = Arc::new(EpochHashMap::new());
    let key_range = 100usize;

    let mut handles = vec![];

    for _ in 0..thread_count {
        let table_clone = Arc::clone(&table);
        let handle = thread::spawn(move || {
            for i in 0..iteration_count {
                let key = i % key_range;
                if i % 2 == 0 {
                    table_clone.insert(key, format!("value_{}", i));
                } else {
                    table_clone.remove(&key);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn skip_map_contention(thread_count: usize, iteration_count: usize) {
    let map: Arc<EpochSkipMap<usize, String>> = Arc::new(EpochSkipMap::new());
    let key_range = 100usize;

    let mut handles = vec![];

    for _ in 0..thread_count {
        let m = Arc::clone(&map);
        let handle = thread::spawn(move || {
            for i in 0..iteration_count {
                let key = i % key_range;
                if i % 2 == 0 {
                    m.insert(key, format!("value_{}", i));
                } else {
                    m.remove(&key);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

// ============================================================================
// Sequential (single-threaded) benchmarks — use usize values to avoid
// String allocation noise dominating the measurement.
// ============================================================================

fn sequential_insert_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_sequential_insert");

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let map: EpochHashMap<usize, usize> = EpochHashMap::new();
                    for i in 0..size {
                        map.insert(i, i);
                    }
                    black_box(&map);
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
                    for i in 0..size {
                        map.insert(i, i);
                    }
                    black_box(&map);
                })
            },
        );
    }

    group.finish();
}

fn sequential_find_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_sequential_find");

    for size in [10_000, 50_000, 100_000, 500_000] {
        // Hit
        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map_hit", size),
            &size,
            |b, &size| {
                let map: EpochHashMap<usize, usize> = EpochHashMap::new();
                for i in 0..size {
                    map.insert(i, i);
                }
                b.iter(|| {
                    for i in 0..size {
                        black_box(map.contains(&i));
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair_hit", size),
            &size,
            |b, &size| {
                let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
                for i in 0..size {
                    map.insert(i, i);
                }
                b.iter(|| {
                    for i in 0..size {
                        black_box(map.contains(&i));
                    }
                })
            },
        );

        // Miss
        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map_miss", size),
            &size,
            |b, &size| {
                let map: EpochHashMap<usize, usize> = EpochHashMap::new();
                for i in 0..size {
                    map.insert(i, i);
                }
                b.iter(|| {
                    for i in size..2 * size {
                        black_box(map.contains(&i));
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair_miss", size),
            &size,
            |b, &size| {
                let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
                for i in 0..size {
                    map.insert(i, i);
                }
                b.iter(|| {
                    for i in size..2 * size {
                        black_box(map.contains(&i));
                    }
                })
            },
        );
    }

    group.finish();
}

fn sequential_remove_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_sequential_remove");

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let map: EpochHashMap<usize, usize> = EpochHashMap::new();
                        for i in 0..size {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size {
                            black_box(map.remove(&i));
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
                        for i in 0..size {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size {
                            black_box(map.remove(&i));
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );
    }

    group.finish();
}

fn sequential_mixed_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_sequential_mixed");

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let map: EpochHashMap<usize, usize> = EpochHashMap::new();
                        for i in 0..size / 2 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size {
                            match i % 3 {
                                0 => {
                                    black_box(map.insert(i + size, i));
                                }
                                1 => {
                                    black_box(map.contains(&(i / 2)));
                                }
                                2 => {
                                    black_box(map.remove(&(i / 3)));
                                }
                                _ => unreachable!(),
                            }
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
                        for i in 0..size / 2 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size {
                            match i % 3 {
                                0 => {
                                    black_box(map.insert(i + size, i));
                                }
                                1 => {
                                    black_box(map.contains(&(i / 2)));
                                }
                                2 => {
                                    black_box(map.remove(&(i / 3)));
                                }
                                _ => unreachable!(),
                            }
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );
    }

    group.finish();
}

fn sequential_update_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_sequential_update");

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let map: EpochHashMap<usize, usize> = EpochHashMap::new();
                        for i in 0..size {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size {
                            black_box(map.update(i, i + 1));
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
                        for i in 0..size {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size {
                            black_box(map.update(i, i + 1));
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );
    }

    group.finish();
}

// ============================================================================
// Concurrent benchmark groups
// ============================================================================

fn concurrent_insert_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_concurrent_insert");
    group.sample_size(10);

    for thread_count in thread_counts() {
        let bench_name = format!("split_ordered_hash_map_{:0>3}_10000", thread_count);
        group.bench_function(&bench_name, |b| {
            b.iter(|| split_ordered_hash_table_insert(black_box(thread_count), black_box(10_000)))
        });

        let bench_name = format!("skip_list_pair_{:0>3}_10000", thread_count);
        group.bench_function(&bench_name, |b| {
            b.iter(|| skip_map_insert(black_box(thread_count), black_box(10_000)))
        });
    }

    group.finish();
}

fn mixed_operations_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_mixed_operations");
    group.sample_size(10);

    for thread_count in thread_counts() {
        let bench_name = format!("split_ordered_hash_map_{:0>3}_10000", thread_count);
        group.bench_function(&bench_name, |b| {
            b.iter_batched(
                || setup_hash_map_mixed(thread_count, 10_000),
                |table| run_hash_map_mixed(table, black_box(thread_count), black_box(10_000)),
                BatchSize::PerIteration,
            )
        });

        let bench_name = format!("skip_list_pair_{:0>3}_10000", thread_count);
        group.bench_function(&bench_name, |b| {
            b.iter_batched(
                || setup_skip_map_mixed(thread_count, 10_000),
                |map| run_skip_map_mixed(map, black_box(thread_count), black_box(10_000)),
                BatchSize::PerIteration,
            )
        });
    }

    group.finish();
}

fn contention_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_high_contention");
    group.sample_size(10);

    for thread_count in thread_counts() {
        let bench_name = format!("split_ordered_hash_map_{:0>3}_10000", thread_count);
        group.bench_function(&bench_name, |b| {
            b.iter(|| {
                split_ordered_hash_table_contention(black_box(thread_count), black_box(10_000))
            })
        });

        let bench_name = format!("skip_list_pair_{:0>3}_10000", thread_count);
        group.bench_function(&bench_name, |b| {
            b.iter(|| skip_map_contention(black_box(thread_count), black_box(10_000)))
        });
    }

    group.finish();
}

fn concurrent_update_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_concurrent_update");
    group.sample_size(10);

    for thread_count in thread_counts() {
        let bench_name = format!("split_ordered_hash_map_{:0>3}_10000", thread_count);
        group.bench_function(&bench_name, |b| {
            b.iter_batched(
                || setup_hash_map_update(thread_count, 10_000),
                |table| run_hash_map_update(table, black_box(thread_count), black_box(10_000)),
                BatchSize::PerIteration,
            )
        });

        let bench_name = format!("skip_list_pair_{:0>3}_10000", thread_count);
        group.bench_function(&bench_name, |b| {
            b.iter_batched(
                || setup_skip_map_update(thread_count, 10_000),
                |map| run_skip_map_update(map, black_box(thread_count), black_box(10_000)),
                BatchSize::PerIteration,
            )
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    sequential_insert_benchmark,
    sequential_find_benchmark,
    sequential_remove_benchmark,
    sequential_update_benchmark,
    sequential_mixed_benchmark,
    concurrent_insert_benchmark,
    concurrent_update_benchmark,
    mixed_operations_benchmark,
    contention_benchmark
);
criterion_main!(benches);
