//! Benchmark comparing YFastTrie against other key-value map implementations:
//! - SplitOrderedHashMap, SkipList<MapEntry>, crossbeam-skiplist SkipMap
//!
//! Run with: cargo bench --package starfish-crossbeam --bench y_fast_trie_benchmark

use criterion::BatchSize;
use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::Throughput;
use criterion::black_box;
use criterion::criterion_group;
use criterion::criterion_main;
use crossbeam_skiplist::SkipMap;
use mimalloc::MiMalloc;
use std::sync::Arc;
use std::thread;

use starfish_core::common_tests::bench_utils::thread_counts;
use starfish_core::data_structures::SplitOrderedHashMap;
use starfish_core::data_structures::hash::MapCollection;
use starfish_core::data_structures::map_entry::MapEntry;
use starfish_core::data_structures::sorted::skip_list::SkipList;
use starfish_core::data_structures::trie::y_fast_trie::YFastTrie;
use starfish_crossbeam::EpochGuard;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// Type aliases
type EpochYFastTrie = YFastTrie<i64, i64, EpochGuard>;
type EpochHashMap = SplitOrderedHashMap<i64, i64, EpochGuard>;
type EpochSkipMap = SkipList<MapEntry<i64, i64>, EpochGuard>;

// ============================================================================
// Sequential benchmarks
// ============================================================================

fn sequential_insert_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("y_fast_trie_sequential_insert");
    group.sample_size(30);

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("y_fast_trie", size), &size, |b, &size| {
            b.iter(|| {
                let trie = EpochYFastTrie::new();
                for i in 0..size as i64 {
                    trie.insert(i, i);
                }
                black_box(&trie);
            })
        });

        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let map: EpochHashMap = EpochHashMap::new();
                    for i in 0..size as i64 {
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
                    let map: EpochSkipMap = EpochSkipMap::new();
                    for i in 0..size as i64 {
                        map.insert(i, i);
                    }
                    black_box(&map);
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam_skipmap", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let map = SkipMap::new();
                    for i in 0..size as i64 {
                        map.insert(i, i);
                    }
                    black_box(&map);
                })
            },
        );
    }

    group.finish();
}

fn sequential_find_hit_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("y_fast_trie_sequential_find_hit");
    group.sample_size(30);

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("y_fast_trie", size), &size, |b, &size| {
            let trie = EpochYFastTrie::new();
            for i in 0..size as i64 {
                trie.insert(i, i);
            }
            b.iter(|| {
                for i in 0..size as i64 {
                    black_box(trie.contains_key(&i));
                }
            })
        });

        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", size),
            &size,
            |b, &size| {
                let map: EpochHashMap = EpochHashMap::new();
                for i in 0..size as i64 {
                    map.insert(i, i);
                }
                b.iter(|| {
                    for i in 0..size as i64 {
                        black_box(map.contains(&i));
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair", size),
            &size,
            |b, &size| {
                let map: EpochSkipMap = EpochSkipMap::new();
                for i in 0..size as i64 {
                    map.insert(i, i);
                }
                b.iter(|| {
                    for i in 0..size as i64 {
                        black_box(map.contains(&i));
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam_skipmap", size),
            &size,
            |b, &size| {
                let map = SkipMap::new();
                for i in 0..size as i64 {
                    map.insert(i, i);
                }
                b.iter(|| {
                    for i in 0..size as i64 {
                        black_box(map.contains_key(&i));
                    }
                })
            },
        );
    }

    group.finish();
}

fn sequential_find_miss_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("y_fast_trie_sequential_find_miss");
    group.sample_size(30);

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("y_fast_trie", size), &size, |b, &size| {
            let trie = EpochYFastTrie::new();
            for i in 0..size as i64 {
                trie.insert(i, i);
            }
            let n = size as i64;
            b.iter(|| {
                for i in n..2 * n {
                    black_box(trie.contains_key(&i));
                }
            })
        });

        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", size),
            &size,
            |b, &size| {
                let map: EpochHashMap = EpochHashMap::new();
                for i in 0..size as i64 {
                    map.insert(i, i);
                }
                let n = size as i64;
                b.iter(|| {
                    for i in n..2 * n {
                        black_box(map.contains(&i));
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair", size),
            &size,
            |b, &size| {
                let map: EpochSkipMap = EpochSkipMap::new();
                for i in 0..size as i64 {
                    map.insert(i, i);
                }
                let n = size as i64;
                b.iter(|| {
                    for i in n..2 * n {
                        black_box(map.contains(&i));
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam_skipmap", size),
            &size,
            |b, &size| {
                let map = SkipMap::new();
                for i in 0..size as i64 {
                    map.insert(i, i);
                }
                let n = size as i64;
                b.iter(|| {
                    for i in n..2 * n {
                        black_box(map.contains_key(&i));
                    }
                })
            },
        );
    }

    group.finish();
}

fn sequential_remove_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("y_fast_trie_sequential_remove");
    group.sample_size(30);

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("y_fast_trie", size), &size, |b, &size| {
            b.iter_batched(
                || {
                    let trie = EpochYFastTrie::new();
                    for i in 0..size as i64 {
                        trie.insert(i, i);
                    }
                    trie
                },
                |trie| {
                    for i in 0..size as i64 {
                        black_box(trie.remove(&i));
                    }
                },
                BatchSize::PerIteration,
            )
        });

        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let map: EpochHashMap = EpochHashMap::new();
                        for i in 0..size as i64 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size as i64 {
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
                        let map: EpochSkipMap = EpochSkipMap::new();
                        for i in 0..size as i64 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size as i64 {
                            black_box(map.remove(&i));
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam_skipmap", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let map = SkipMap::new();
                        for i in 0..size as i64 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size as i64 {
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
    let mut group = c.benchmark_group("y_fast_trie_sequential_mixed");
    group.sample_size(30);

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("y_fast_trie", size), &size, |b, &size| {
            b.iter_batched(
                || {
                    let trie = EpochYFastTrie::new();
                    for i in 0..size as i64 / 2 {
                        trie.insert(i, i);
                    }
                    trie
                },
                |trie| {
                    for i in 0..size as i64 {
                        match i % 3 {
                            0 => {
                                black_box(trie.insert(i + size as i64, i));
                            }
                            1 => {
                                black_box(trie.contains_key(&(i / 2)));
                            }
                            _ => {
                                black_box(trie.remove(&(i / 3)));
                            }
                        }
                    }
                },
                BatchSize::PerIteration,
            )
        });

        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let map: EpochHashMap = EpochHashMap::new();
                        for i in 0..size as i64 / 2 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size as i64 {
                            match i % 3 {
                                0 => {
                                    black_box(map.insert(i + size as i64, i));
                                }
                                1 => {
                                    black_box(map.contains(&(i / 2)));
                                }
                                _ => {
                                    black_box(map.remove(&(i / 3)));
                                }
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
                        let map: EpochSkipMap = EpochSkipMap::new();
                        for i in 0..size as i64 / 2 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size as i64 {
                            match i % 3 {
                                0 => {
                                    black_box(map.insert(i + size as i64, i));
                                }
                                1 => {
                                    black_box(map.contains(&(i / 2)));
                                }
                                _ => {
                                    black_box(map.remove(&(i / 3)));
                                }
                            }
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam_skipmap", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let map = SkipMap::new();
                        for i in 0..size as i64 / 2 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        for i in 0..size as i64 {
                            match i % 3 {
                                0 => {
                                    black_box(map.insert(i + size as i64, i));
                                }
                                1 => {
                                    black_box(map.contains_key(&(i / 2)));
                                }
                                _ => {
                                    black_box(map.remove(&(i / 3)));
                                }
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

// ============================================================================
// Concurrent benchmarks
// ============================================================================

const CONCURRENT_OPS_PER_THREAD: usize = 10_000;

fn concurrent_insert_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("y_fast_trie_concurrent_insert");

    for threads in thread_counts() {
        group.throughput(Throughput::Elements(
            (threads * CONCURRENT_OPS_PER_THREAD) as u64,
        ));

        group.bench_with_input(
            BenchmarkId::new("y_fast_trie", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    let trie = Arc::new(EpochYFastTrie::new());
                    let handles: Vec<_> = (0..threads)
                        .map(|t| {
                            let trie = Arc::clone(&trie);
                            thread::spawn(move || {
                                let base = (t * CONCURRENT_OPS_PER_THREAD) as i64;
                                for i in 0..CONCURRENT_OPS_PER_THREAD as i64 {
                                    trie.insert(base + i, base + i);
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                    black_box(&trie);
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    let map: Arc<EpochHashMap> = Arc::new(EpochHashMap::new());
                    let handles: Vec<_> = (0..threads)
                        .map(|t| {
                            let map = Arc::clone(&map);
                            thread::spawn(move || {
                                let base = (t * CONCURRENT_OPS_PER_THREAD) as i64;
                                for i in 0..CONCURRENT_OPS_PER_THREAD as i64 {
                                    map.insert(base + i, base + i);
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                    black_box(&map);
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    let map: Arc<EpochSkipMap> = Arc::new(EpochSkipMap::new());
                    let handles: Vec<_> = (0..threads)
                        .map(|t| {
                            let map = Arc::clone(&map);
                            thread::spawn(move || {
                                let base = (t * CONCURRENT_OPS_PER_THREAD) as i64;
                                for i in 0..CONCURRENT_OPS_PER_THREAD as i64 {
                                    map.insert(base + i, base + i);
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                    black_box(&map);
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam_skipmap", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    let map: Arc<SkipMap<i64, i64>> = Arc::new(SkipMap::new());
                    let handles: Vec<_> = (0..threads)
                        .map(|t| {
                            let map = Arc::clone(&map);
                            thread::spawn(move || {
                                let base = (t * CONCURRENT_OPS_PER_THREAD) as i64;
                                for i in 0..CONCURRENT_OPS_PER_THREAD as i64 {
                                    map.insert(base + i, base + i);
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                    black_box(&map);
                })
            },
        );
    }

    group.finish();
}

fn concurrent_mixed_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("y_fast_trie_concurrent_mixed");

    for threads in thread_counts() {
        group.throughput(Throughput::Elements(
            (threads * CONCURRENT_OPS_PER_THREAD) as u64,
        ));

        group.bench_with_input(
            BenchmarkId::new("y_fast_trie", threads),
            &threads,
            |b, &threads| {
                b.iter_batched(
                    || {
                        let trie = Arc::new(EpochYFastTrie::new());
                        for i in 0..(threads * CONCURRENT_OPS_PER_THREAD / 2) as i64 {
                            trie.insert(i, i);
                        }
                        trie
                    },
                    |trie| {
                        let handles: Vec<_> = (0..threads)
                            .map(|t| {
                                let trie = Arc::clone(&trie);
                                thread::spawn(move || {
                                    let base = (t * CONCURRENT_OPS_PER_THREAD) as i64;
                                    for i in 0..CONCURRENT_OPS_PER_THREAD {
                                        match i % 3 {
                                            0 => {
                                                trie.insert(base + i as i64 + 1_000_000, i as i64);
                                            }
                                            1 => {
                                                black_box(trie.contains_key(&(i as i64 / 2)));
                                            }
                                            _ => {
                                                trie.remove(&(i as i64 / 2));
                                            }
                                        }
                                    }
                                })
                            })
                            .collect();
                        for h in handles {
                            h.join().unwrap();
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", threads),
            &threads,
            |b, &threads| {
                b.iter_batched(
                    || {
                        let map: Arc<EpochHashMap> = Arc::new(EpochHashMap::new());
                        for i in 0..(threads * CONCURRENT_OPS_PER_THREAD / 2) as i64 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        let handles: Vec<_> = (0..threads)
                            .map(|t| {
                                let map = Arc::clone(&map);
                                thread::spawn(move || {
                                    let base = (t * CONCURRENT_OPS_PER_THREAD) as i64;
                                    for i in 0..CONCURRENT_OPS_PER_THREAD {
                                        match i % 3 {
                                            0 => {
                                                map.insert(base + i as i64 + 1_000_000, i as i64);
                                            }
                                            1 => {
                                                black_box(map.contains(&(i as i64 / 2)));
                                            }
                                            _ => {
                                                map.remove(&(i as i64 / 2));
                                            }
                                        }
                                    }
                                })
                            })
                            .collect();
                        for h in handles {
                            h.join().unwrap();
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair", threads),
            &threads,
            |b, &threads| {
                b.iter_batched(
                    || {
                        let map: Arc<EpochSkipMap> = Arc::new(EpochSkipMap::new());
                        for i in 0..(threads * CONCURRENT_OPS_PER_THREAD / 2) as i64 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        let handles: Vec<_> = (0..threads)
                            .map(|t| {
                                let map = Arc::clone(&map);
                                thread::spawn(move || {
                                    let base = (t * CONCURRENT_OPS_PER_THREAD) as i64;
                                    for i in 0..CONCURRENT_OPS_PER_THREAD {
                                        match i % 3 {
                                            0 => {
                                                map.insert(base + i as i64 + 1_000_000, i as i64);
                                            }
                                            1 => {
                                                black_box(map.contains(&(i as i64 / 2)));
                                            }
                                            _ => {
                                                map.remove(&(i as i64 / 2));
                                            }
                                        }
                                    }
                                })
                            })
                            .collect();
                        for h in handles {
                            h.join().unwrap();
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam_skipmap", threads),
            &threads,
            |b, &threads| {
                b.iter_batched(
                    || {
                        let map: Arc<SkipMap<i64, i64>> = Arc::new(SkipMap::new());
                        for i in 0..(threads * CONCURRENT_OPS_PER_THREAD / 2) as i64 {
                            map.insert(i, i);
                        }
                        map
                    },
                    |map| {
                        let handles: Vec<_> = (0..threads)
                            .map(|t| {
                                let map = Arc::clone(&map);
                                thread::spawn(move || {
                                    let base = (t * CONCURRENT_OPS_PER_THREAD) as i64;
                                    for i in 0..CONCURRENT_OPS_PER_THREAD {
                                        match i % 3 {
                                            0 => {
                                                map.insert(base + i as i64 + 1_000_000, i as i64);
                                            }
                                            1 => {
                                                black_box(map.contains_key(&(i as i64 / 2)));
                                            }
                                            _ => {
                                                map.remove(&(i as i64 / 2));
                                            }
                                        }
                                    }
                                })
                            })
                            .collect();
                        for h in handles {
                            h.join().unwrap();
                        }
                    },
                    BatchSize::PerIteration,
                )
            },
        );
    }

    group.finish();
}

fn concurrent_contention_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("y_fast_trie_concurrent_contention");

    let key_range = 100i64;

    for threads in thread_counts() {
        group.throughput(Throughput::Elements(
            (threads * CONCURRENT_OPS_PER_THREAD) as u64,
        ));

        group.bench_with_input(
            BenchmarkId::new("y_fast_trie", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    let trie = Arc::new(EpochYFastTrie::new());
                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let trie = Arc::clone(&trie);
                            thread::spawn(move || {
                                for i in 0..CONCURRENT_OPS_PER_THREAD {
                                    let key = (i as i64) % key_range;
                                    if i % 2 == 0 {
                                        trie.insert(key, key);
                                    } else {
                                        trie.remove(&key);
                                    }
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("split_ordered_hash_map", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    let map: Arc<EpochHashMap> = Arc::new(EpochHashMap::new());
                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let map = Arc::clone(&map);
                            thread::spawn(move || {
                                for i in 0..CONCURRENT_OPS_PER_THREAD {
                                    let key = (i as i64) % key_range;
                                    if i % 2 == 0 {
                                        map.insert(key, key);
                                    } else {
                                        map.remove(&key);
                                    }
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("skip_list_pair", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    let map: Arc<EpochSkipMap> = Arc::new(EpochSkipMap::new());
                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let map = Arc::clone(&map);
                            thread::spawn(move || {
                                for i in 0..CONCURRENT_OPS_PER_THREAD {
                                    let key = (i as i64) % key_range;
                                    if i % 2 == 0 {
                                        map.insert(key, key);
                                    } else {
                                        map.remove(&key);
                                    }
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam_skipmap", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    let map: Arc<SkipMap<i64, i64>> = Arc::new(SkipMap::new());
                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let map = Arc::clone(&map);
                            thread::spawn(move || {
                                for i in 0..CONCURRENT_OPS_PER_THREAD {
                                    let key = (i as i64) % key_range;
                                    if i % 2 == 0 {
                                        map.insert(key, key);
                                    } else {
                                        map.remove(&key);
                                    }
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                })
            },
        );
    }

    group.finish();
}

// ============================================================================
// Bucket size sensitivity benchmark
// ============================================================================

fn bucket_size_insert_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("y_fast_trie_bucket_size_insert");
    group.sample_size(20);

    for &bucket_max in &[60, 120, 240, 480] {
        for &size in &[10_000, 100_000, 500_000] {
            group.throughput(Throughput::Elements(size as u64));

            group.bench_with_input(
                BenchmarkId::new(format!("bmax_{}", bucket_max), size),
                &size,
                |b, &size| {
                    b.iter(|| {
                        let trie = EpochYFastTrie::with_bucket_max(bucket_max);
                        for i in 0..size as i64 {
                            trie.insert(i, i);
                        }
                        black_box(&trie);
                    })
                },
            );
        }
    }

    group.finish();
}

fn bucket_size_find_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("y_fast_trie_bucket_size_find");
    group.sample_size(20);

    let size = 500_000usize;
    group.throughput(Throughput::Elements(size as u64));

    for &bucket_max in &[60, 120, 240, 480] {
        let trie = EpochYFastTrie::with_bucket_max(bucket_max);
        for i in 0..size as i64 {
            trie.insert(i, i);
        }

        group.bench_with_input(
            BenchmarkId::new(format!("bmax_{}_hit", bucket_max), size),
            &size,
            |b, &size| {
                b.iter(|| {
                    for i in 0..size as i64 {
                        black_box(trie.contains_key(&i));
                    }
                })
            },
        );
    }

    group.finish();
}

fn bucket_size_mixed_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("y_fast_trie_bucket_size_mixed");
    group.sample_size(10);

    let size = 100_000usize;
    group.throughput(Throughput::Elements(size as u64));

    for &bucket_max in &[8, 16, 32, 60] {
        group.bench_with_input(
            BenchmarkId::new(format!("bmax_{}", bucket_max), size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || {
                        let trie = EpochYFastTrie::with_bucket_max(bucket_max);
                        for i in 0..size as i64 / 2 {
                            trie.insert(i, i);
                        }
                        trie
                    },
                    |trie| {
                        for i in 0..size as i64 {
                            match i % 3 {
                                0 => {
                                    black_box(trie.insert(i + size as i64, i));
                                }
                                1 => {
                                    black_box(trie.contains_key(&(i / 2)));
                                }
                                _ => {
                                    black_box(trie.remove(&(i / 3)));
                                }
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

criterion_group!(
    benches,
    sequential_insert_benchmark,
    sequential_find_hit_benchmark,
    sequential_find_miss_benchmark,
    sequential_remove_benchmark,
    sequential_mixed_benchmark,
    concurrent_insert_benchmark,
    concurrent_mixed_benchmark,
    concurrent_contention_benchmark,
    bucket_size_insert_benchmark,
    bucket_size_find_benchmark,
    bucket_size_mixed_benchmark,
);
criterion_main!(benches);
