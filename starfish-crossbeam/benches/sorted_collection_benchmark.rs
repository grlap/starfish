//! Benchmark comparing SortedCollection implementations:
//! - SkipList, SortedList vs crossbeam-skiplist
//!
//! Run with: cargo bench --package starfish-crossbeam --bench sorted_collection_benchmark

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
use starfish_core::data_structures::Ordered;
use starfish_core::data_structures::SkipList;
use starfish_core::data_structures::SkipTrie;
use starfish_core::data_structures::SortedCollection;
use starfish_core::data_structures::SortedList;
use starfish_crossbeam::EpochGuard;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const OPS_PER_THREAD: usize = 10_000;

// Type aliases for convenience
type EpochSkipList = SkipList<i64, EpochGuard>;
type EpochSortedList = SortedList<i64, EpochGuard>;
type EpochSkipTrie = SkipTrie<i64, EpochGuard>;

// ============================================================================
// Generic benchmark helpers for SortedCollection
// ============================================================================

/// Generic sequential update benchmark - works with any SortedCollection
fn bench_update<C>(list: &C, count: usize, update_iterations: usize)
where
    C: SortedCollection<i64>,
{
    // Pre-populate
    for i in 0..count {
        list.insert(i as i64);
    }

    // Perform updates
    for _ in 0..update_iterations {
        for i in 0..count {
            list.update(i as i64);
        }
    }
}

/// Generic delete+insert benchmark - works with any SortedCollection
fn bench_delete_insert<C>(list: &C, count: usize, update_iterations: usize)
where
    C: SortedCollection<i64>,
{
    // Pre-populate
    for i in 0..count {
        list.insert(i as i64);
    }

    // Perform updates via delete + insert
    for _ in 0..update_iterations {
        for i in 0..count {
            list.delete(&(i as i64));
            list.insert(i as i64);
        }
    }
}

/// Generic concurrent update benchmark - works with any SortedCollection
fn bench_concurrent_update<C>(list: Arc<C>, thread_count: usize, ops_per_thread: usize)
where
    C: SortedCollection<i64> + Send + Sync + 'static,
{
    // Pre-populate with values that each thread will update
    let total_keys = thread_count * 100;
    for i in 0..total_keys {
        list.insert(i as i64);
    }

    let mut handles = vec![];

    for t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            let base = (t * 100) as i64;
            for _ in 0..ops_per_thread {
                for j in 0..100 {
                    list_clone.update(base + j);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

/// Generic concurrent delete+insert benchmark - works with any SortedCollection
fn bench_concurrent_delete_insert<C>(list: Arc<C>, thread_count: usize, ops_per_thread: usize)
where
    C: SortedCollection<i64> + Send + Sync + 'static,
{
    // Pre-populate with distinct key ranges for each thread
    let keys_per_thread = 100;
    for t in 0..thread_count {
        let base = (t * keys_per_thread * 2) as i64; // Spread out to avoid overlap
        for j in 0..keys_per_thread {
            list.insert(base + j as i64);
        }
    }

    let mut handles = vec![];

    for t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            let base = (t * keys_per_thread * 2) as i64;
            for _ in 0..ops_per_thread {
                for j in 0..keys_per_thread {
                    list_clone.delete(&(base + j as i64));
                    list_clone.insert(base + j as i64);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

/// Generic high contention update benchmark - works with any SortedCollection
fn bench_high_contention<C>(list: Arc<C>, thread_count: usize, ops_per_thread: usize)
where
    C: SortedCollection<i64> + Send + Sync + 'static,
{
    // Pre-populate with small key range
    let key_range = 50i64;
    for i in 0..key_range {
        list.insert(i);
    }

    let mut handles = vec![];

    for _ in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            for i in 0..ops_per_thread {
                let key = (i as i64) % key_range;
                list_clone.update(key);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

// ============================================================================
// Sequential (single-threaded) benchmarks
// ============================================================================

fn shuffled_keys(n: usize) -> Vec<i64> {
    let mut keys: Vec<i64> = (0..n as i64).collect();
    for i in (1..keys.len()).rev() {
        keys.swap(i, fastrand::usize(..=i));
    }
    keys
}

fn sequential_insert_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("sequential_insert_sorted_collection");
    group.sample_size(30);

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("skiplist", size), &size, |b, &size| {
            b.iter(|| {
                let list = EpochSkipList::default();
                for i in 0..size as i64 {
                    list.insert(i);
                }
                black_box(&list);
            })
        });

        group.bench_with_input(
            BenchmarkId::new("skiptrie", size),
            &size,
            |b, &size: &usize| {
                b.iter(|| {
                    let trie = EpochSkipTrie::with_universe_size(size.next_power_of_two());
                    for i in 0..size as i64 {
                        trie.insert(i);
                    }
                    black_box(&trie);
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam", size),
            &size,
            |b, &size: &usize| {
                b.iter(|| {
                    let map = SkipMap::new();
                    for i in 0..size as i64 {
                        map.insert(i, ());
                    }
                    black_box(&map);
                })
            },
        );
    }

    group.finish();
}

fn random_insert_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("random_insert_sorted_collection");
    group.sample_size(30);

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));
        let keys = shuffled_keys(size);

        group.bench_with_input(BenchmarkId::new("skiplist", size), &size, |b, _| {
            b.iter(|| {
                let list = EpochSkipList::default();
                for &k in &keys {
                    list.insert(k);
                }
                black_box(&list);
            })
        });

        group.bench_with_input(
            BenchmarkId::new("skiptrie", size),
            &size,
            |b, &size: &usize| {
                b.iter(|| {
                    let trie = EpochSkipTrie::with_universe_size(size.next_power_of_two());
                    for &k in &keys {
                        trie.insert(k);
                    }
                    black_box(&trie);
                })
            },
        );

        group.bench_with_input(BenchmarkId::new("crossbeam", size), &size, |b, _| {
            b.iter(|| {
                let map = SkipMap::new();
                for &k in &keys {
                    map.insert(k, ());
                }
                black_box(&map);
            })
        });
    }

    group.finish();
}

fn sequential_find_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("sequential_find_sorted_collection");
    group.sample_size(30);

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("skiplist", size), &size, |b, &size| {
            let list = EpochSkipList::default();
            for i in 0..size as i64 {
                list.insert(i);
            }
            b.iter(|| {
                for i in 0..size as i64 {
                    black_box(list.contains(&i));
                }
            })
        });

        group.bench_with_input(
            BenchmarkId::new("skiptrie", size),
            &size,
            |b, &size: &usize| {
                let trie = EpochSkipTrie::with_universe_size(size.next_power_of_two());
                for i in 0..size as i64 {
                    trie.insert(i);
                }
                b.iter(|| {
                    for i in 0..size as i64 {
                        black_box(trie.contains(&i));
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam", size),
            &size,
            |b, &size: &usize| {
                let map = SkipMap::new();
                for i in 0..size as i64 {
                    map.insert(i, ());
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

fn random_find_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("random_find_sorted_collection");
    group.sample_size(30);

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));
        let keys = shuffled_keys(size);

        group.bench_with_input(BenchmarkId::new("skiplist", size), &size, |b, &size| {
            let list = EpochSkipList::default();
            for i in 0..size as i64 {
                list.insert(i);
            }
            b.iter(|| {
                for &k in &keys {
                    black_box(list.contains(&k));
                }
            })
        });

        group.bench_with_input(
            BenchmarkId::new("skiptrie", size),
            &size,
            |b, &size: &usize| {
                let trie = EpochSkipTrie::with_universe_size(size.next_power_of_two());
                for i in 0..size as i64 {
                    trie.insert(i);
                }
                b.iter(|| {
                    for &k in &keys {
                        black_box(trie.contains(&k));
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("crossbeam", size),
            &size,
            |b, &size: &usize| {
                let map = SkipMap::new();
                for i in 0..size as i64 {
                    map.insert(i, ());
                }
                b.iter(|| {
                    for &k in &keys {
                        black_box(map.contains_key(&k));
                    }
                })
            },
        );
    }

    group.finish();
}

fn find_miss_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("find_miss_sorted_collection");
    group.sample_size(10);

    for size in [10_000, 50_000, 100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("skiplist", size), &size, |b, &size| {
            let list = EpochSkipList::default();
            for i in 0..size as i64 {
                list.insert(i);
            }
            b.iter(|| {
                let n = size as i64;
                for i in n..2 * n {
                    black_box(list.contains(&i));
                }
            })
        });

        // SkipTrie find-miss is pathologically slow at large sizes (~156s/iter at 500K)
        // due to full PrefixTable scan on missing keys. Cap at 100K.
        if size <= 100_000 {
            group.bench_with_input(
                BenchmarkId::new("skiptrie", size),
                &size,
                |b, &size: &usize| {
                    let universe = (size * 2).next_power_of_two();
                    let trie = EpochSkipTrie::with_universe_size(universe);
                    for i in 0..size as i64 {
                        trie.insert(i);
                    }
                    b.iter(|| {
                        let n = size as i64;
                        for i in n..2 * n {
                            black_box(trie.contains(&i));
                        }
                    })
                },
            );
        }

        group.bench_with_input(BenchmarkId::new("crossbeam", size), &size, |b, &size| {
            let map = SkipMap::new();
            for i in 0..size as i64 {
                map.insert(i, ());
            }
            b.iter(|| {
                let n = size as i64;
                for i in n..2 * n {
                    black_box(map.contains_key(&i));
                }
            })
        });
    }

    group.finish();
}

fn mixed_contention_sequential_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_contention_sequential_sorted_collection");
    group.sample_size(30);
    let key_range = 100i64;

    for size in [100_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("skiplist", size), &size, |b, &size| {
            b.iter(|| {
                let list = EpochSkipList::default();
                for i in 0..key_range {
                    list.insert(i);
                }
                for i in 0..size as i64 {
                    let key = i % key_range;
                    if i % 2 == 0 {
                        list.insert(key);
                    } else {
                        list.delete(&key);
                    }
                }
                black_box(&list);
            })
        });

        group.bench_with_input(BenchmarkId::new("skiptrie", size), &size, |b, _| {
            b.iter(|| {
                let trie = EpochSkipTrie::with_universe_size(128);
                for i in 0..key_range {
                    trie.insert(i);
                }
                for i in 0..size as i64 {
                    let key = i % key_range;
                    if i % 2 == 0 {
                        trie.insert(key);
                    } else {
                        trie.delete(&key);
                    }
                }
                black_box(&trie);
            })
        });

        group.bench_with_input(BenchmarkId::new("crossbeam", size), &size, |b, &size| {
            b.iter(|| {
                let map = SkipMap::new();
                for i in 0..key_range {
                    map.insert(i, ());
                }
                for i in 0..size as i64 {
                    let key = i % key_range;
                    if i % 2 == 0 {
                        map.insert(key, ());
                    } else {
                        map.remove(&key);
                    }
                }
                black_box(&map);
            })
        });
    }

    group.finish();
}

// ============================================================================
// Concurrent insert-only benchmarks
// ============================================================================

fn bench_starfish_skiplist_insert(thread_count: usize, ops_per_thread: usize) {
    let list: Arc<EpochSkipList> = Arc::new(EpochSkipList::default());
    let mut handles = vec![];

    for t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            let base = (t * ops_per_thread) as i64;
            for i in 0..ops_per_thread {
                list_clone.insert(base + i as i64);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn bench_starfish_skiptrie_insert(thread_count: usize, ops_per_thread: usize) {
    let universe = (thread_count * ops_per_thread).next_power_of_two();
    let trie: Arc<EpochSkipTrie> = Arc::new(EpochSkipTrie::with_universe_size(universe));
    let mut handles = vec![];

    for t in 0..thread_count {
        let trie_clone = Arc::clone(&trie);
        let handle = thread::spawn(move || {
            let base = (t * ops_per_thread) as i64;
            for i in 0..ops_per_thread {
                trie_clone.insert(base + i as i64);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn bench_crossbeam_insert(thread_count: usize, ops_per_thread: usize) {
    let map: Arc<SkipMap<i64, ()>> = Arc::new(SkipMap::new());
    let mut handles = vec![];

    for t in 0..thread_count {
        let map_clone = Arc::clone(&map);
        let handle = thread::spawn(move || {
            let base = (t * ops_per_thread) as i64;
            for i in 0..ops_per_thread {
                map_clone.insert(base + i as i64, ());
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

// ============================================================================
// Mixed insert/delete benchmarks (50% insert, 50% delete)
// ============================================================================

fn bench_starfish_skiplist_mixed(thread_count: usize, ops_per_thread: usize) {
    let list: Arc<EpochSkipList> = Arc::new(EpochSkipList::default());

    // Pre-populate with half the values
    for i in 0..(thread_count * ops_per_thread / 2) {
        list.insert(i as i64);
    }

    let mut handles = vec![];

    for t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            let base = (t * ops_per_thread) as i64;
            for i in 0..ops_per_thread {
                if i % 2 == 0 {
                    // Insert new value
                    list_clone.insert(base + i as i64 + 1_000_000);
                } else {
                    // Delete existing value
                    list_clone.delete(&(i as i64 / 2));
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn bench_starfish_skiptrie_mixed(thread_count: usize, ops_per_thread: usize) {
    // Keys up to base + ops_per_thread + 1_000_000 per thread
    let universe = (thread_count * ops_per_thread + 1_100_000).next_power_of_two();
    let trie: Arc<EpochSkipTrie> = Arc::new(EpochSkipTrie::with_universe_size(universe));

    // Pre-populate with half the values
    for i in 0..(thread_count * ops_per_thread / 2) {
        trie.insert(i as i64);
    }

    let mut handles = vec![];

    for t in 0..thread_count {
        let trie_clone = Arc::clone(&trie);
        let handle = thread::spawn(move || {
            let base = (t * ops_per_thread) as i64;
            for i in 0..ops_per_thread {
                if i % 2 == 0 {
                    trie_clone.insert(base + i as i64 + 1_000_000);
                } else {
                    trie_clone.delete(&(i as i64 / 2));
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn bench_crossbeam_mixed(thread_count: usize, ops_per_thread: usize) {
    let map: Arc<SkipMap<i64, ()>> = Arc::new(SkipMap::new());

    // Pre-populate with half the values
    for i in 0..(thread_count * ops_per_thread / 2) {
        map.insert(i as i64, ());
    }

    let mut handles = vec![];

    for t in 0..thread_count {
        let map_clone = Arc::clone(&map);
        let handle = thread::spawn(move || {
            let base = (t * ops_per_thread) as i64;
            for i in 0..ops_per_thread {
                if i % 2 == 0 {
                    // Insert new value
                    map_clone.insert(base + i as i64 + 1_000_000, ());
                } else {
                    // Delete existing value
                    map_clone.remove(&(i as i64 / 2));
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
// High contention benchmark (all threads work on same key range)
// ============================================================================

fn bench_starfish_skiplist_contention(thread_count: usize, ops_per_thread: usize) {
    let list: Arc<EpochSkipList> = Arc::new(EpochSkipList::default());
    let mut handles = vec![];

    // Small key range to maximize contention
    let key_range = 100i64;

    for _ in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            for i in 0..ops_per_thread {
                let key = (i as i64) % key_range;
                if i % 2 == 0 {
                    list_clone.insert(key);
                } else {
                    list_clone.delete(&key);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn bench_starfish_skiptrie_contention(thread_count: usize, ops_per_thread: usize) {
    let trie: Arc<EpochSkipTrie> = Arc::new(EpochSkipTrie::with_universe_size(128));
    let mut handles = vec![];

    let key_range = 100i64;

    for _ in 0..thread_count {
        let trie_clone = Arc::clone(&trie);
        let handle = thread::spawn(move || {
            for i in 0..ops_per_thread {
                let key = (i as i64) % key_range;
                if i % 2 == 0 {
                    trie_clone.insert(key);
                } else {
                    trie_clone.delete(&key);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn bench_crossbeam_contention(thread_count: usize, ops_per_thread: usize) {
    let map: Arc<SkipMap<i64, ()>> = Arc::new(SkipMap::new());
    let mut handles = vec![];

    // Small key range to maximize contention
    let key_range = 100i64;

    for _ in 0..thread_count {
        let map_clone = Arc::clone(&map);
        let handle = thread::spawn(move || {
            for i in 0..ops_per_thread {
                let key = (i as i64) % key_range;
                if i % 2 == 0 {
                    map_clone.insert(key, ());
                } else {
                    map_clone.remove(&key);
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
// Criterion benchmark groups
// ============================================================================

fn insert_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_benchmark_sorted_collection");
    group.sample_size(10);

    for threads in thread_counts() {
        group.throughput(Throughput::Elements((threads * OPS_PER_THREAD) as u64));
        group.bench_with_input(
            BenchmarkId::new("insert_benchmark_skiplist", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_starfish_skiplist_insert(black_box(threads), black_box(OPS_PER_THREAD))
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("insert_benchmark_skiptrie", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_starfish_skiptrie_insert(black_box(threads), black_box(OPS_PER_THREAD))
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("insert_benchmark_crossbeam", threads),
            &threads,
            |b, &threads| {
                b.iter(|| bench_crossbeam_insert(black_box(threads), black_box(OPS_PER_THREAD)))
            },
        );
    }

    group.finish();
}

fn mixed_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_benchmark_sorted_collection");
    group.sample_size(10);

    for threads in thread_counts() {
        group.throughput(Throughput::Elements((threads * OPS_PER_THREAD) as u64));
        group.bench_with_input(
            BenchmarkId::new("mixed_benchmark_skiplist", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_starfish_skiplist_mixed(black_box(threads), black_box(OPS_PER_THREAD))
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("mixed_benchmark_skiptrie", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_starfish_skiptrie_mixed(black_box(threads), black_box(OPS_PER_THREAD))
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("mixed_benchmark_crossbeam", threads),
            &threads,
            |b, &threads| {
                b.iter(|| bench_crossbeam_mixed(black_box(threads), black_box(OPS_PER_THREAD)))
            },
        );
    }

    group.finish();
}

fn contention_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention_benchmark_sorted_collection");
    group.sample_size(10);

    for threads in thread_counts() {
        group.throughput(Throughput::Elements((threads * OPS_PER_THREAD) as u64));
        group.bench_with_input(
            BenchmarkId::new("contention_benchmark_skiplist", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_starfish_skiplist_contention(
                        black_box(threads),
                        black_box(OPS_PER_THREAD),
                    )
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("contention_benchmark_skiptrie", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_starfish_skiptrie_contention(
                        black_box(threads),
                        black_box(OPS_PER_THREAD),
                    )
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("contention_benchmark_crossbeam", threads),
            &threads,
            |b, &threads| {
                b.iter(|| bench_crossbeam_contention(black_box(threads), black_box(OPS_PER_THREAD)))
            },
        );
    }

    group.finish();
}

// ============================================================================
// Batch insert benchmarks - comparing sequential vs batch insert
// ============================================================================

/// Sequential insert - one at a time
fn bench_sequential_insert_skiplist(count: usize) {
    let list: EpochSkipList = EpochSkipList::default();
    for i in 0..count {
        list.insert(i as i64);
    }
}

fn bench_sequential_insert_crossbeam(count: usize) {
    let map: SkipMap<i64, ()> = SkipMap::new();
    for i in 0..count {
        map.insert(i as i64, ());
    }
}

/// Batch insert - using insert_batch with Ordered iterator
fn bench_batch_insert_skiplist(count: usize) {
    let list: EpochSkipList = EpochSkipList::default();
    let data: Vec<i64> = (0..count as i64).collect();
    let ordered = Ordered::new(data.into_iter());
    list.insert_batch(ordered);
}

fn bench_batch_insert_skiptrie(count: usize) {
    let trie: EpochSkipTrie = EpochSkipTrie::with_universe_size(count.next_power_of_two());
    let data: Vec<i64> = (0..count as i64).collect();
    let ordered = Ordered::new(data.into_iter());
    trie.insert_batch(ordered);
}

fn bench_batch_insert_sorted_list(count: usize) {
    let list: EpochSortedList = EpochSortedList::default();
    let data: Vec<i64> = (0..count as i64).collect();
    let ordered = Ordered::new(data.into_iter());
    list.insert_batch(ordered);
}

/// Batch insert benchmark group
fn batch_insert_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_insert_benchmark_sorted_collection");
    group.sample_size(10);

    for size in [1_000, 10_000, 100_000, 200_000, 500_000] {
        group.throughput(Throughput::Elements(size as u64));
        // Sequential inserts
        group.bench_with_input(
            BenchmarkId::new("batch_insert_benchmark_skiplist_sequential", size),
            &size,
            |b, &size| b.iter(|| bench_sequential_insert_skiplist(black_box(size))),
        );

        group.bench_with_input(
            BenchmarkId::new("batch_insert_benchmark_crossbeam_sequential", size),
            &size,
            |b, &size| b.iter(|| bench_sequential_insert_crossbeam(black_box(size))),
        );

        // Batch inserts (only for our collections - crossbeam doesn't have batch API)
        group.bench_with_input(
            BenchmarkId::new("batch_insert_benchmark_skiplist_batch", size),
            &size,
            |b, &size| b.iter(|| bench_batch_insert_skiplist(black_box(size))),
        );

        group.bench_with_input(
            BenchmarkId::new("batch_insert_benchmark_sorted_list_batch", size),
            &size,
            |b, &size| b.iter(|| bench_batch_insert_sorted_list(black_box(size))),
        );

        group.bench_with_input(
            BenchmarkId::new("batch_insert_benchmark_skiptrie_sequential", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let trie = EpochSkipTrie::with_universe_size(size.next_power_of_two());
                    for i in 0..size {
                        trie.insert(i as i64);
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("batch_insert_benchmark_skiptrie_batch", size),
            &size,
            |b, &size| b.iter(|| bench_batch_insert_skiptrie(black_box(size))),
        );
    }

    group.finish();
}

/// SkipList-focused benchmark: O(log n) sequential vs O(1) amortized batch
/// Using larger sizes to see the difference more clearly
fn skiplist_batch_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("skiplist_batch_large_benchmark_sorted_collection");
    group.sample_size(10);

    for size in [10_000, 50_000, 100_000, 200_000] {
        group.throughput(Throughput::Elements(size as u64));
        // SkipList - sequential
        group.bench_with_input(
            BenchmarkId::new("skiplist_batch_large_benchmark_skiplist_sequential", size),
            &size,
            |b, &size| b.iter(|| bench_sequential_insert_skiplist(black_box(size))),
        );

        // SkipList - batch
        group.bench_with_input(
            BenchmarkId::new("skiplist_batch_large_benchmark_skiplist_batch", size),
            &size,
            |b, &size| b.iter(|| bench_batch_insert_skiplist(black_box(size))),
        );

        // Crossbeam for comparison (sequential only)
        group.bench_with_input(
            BenchmarkId::new("skiplist_batch_large_benchmark_crossbeam_sequential", size),
            &size,
            |b, &size| b.iter(|| bench_sequential_insert_crossbeam(black_box(size))),
        );
    }

    group.finish();
}

// ============================================================================
// Concurrent batch insert benchmarks
// ============================================================================

fn bench_concurrent_sequential_insert<C>(thread_count: usize, ops_per_thread: usize)
where
    C: SortedCollection<i64> + Default + Send + Sync + 'static,
{
    let list: Arc<C> = Arc::new(C::default());
    let mut handles = vec![];

    for t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            let base = (t * ops_per_thread) as i64;
            for i in 0..ops_per_thread {
                list_clone.insert(base + i as i64);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn bench_concurrent_batch_insert<C>(thread_count: usize, ops_per_thread: usize)
where
    C: SortedCollection<i64> + Default + Send + Sync + 'static,
{
    let list: Arc<C> = Arc::new(C::default());
    let mut handles = vec![];

    for t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            let base = (t * ops_per_thread) as i64;
            let data: Vec<i64> = (base..base + ops_per_thread as i64).collect();
            let ordered = Ordered::new(data.into_iter());
            list_clone.insert_batch(ordered);
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn bench_concurrent_sequential_insert_skiptrie(thread_count: usize, ops_per_thread: usize) {
    let universe = (thread_count * ops_per_thread).next_power_of_two();
    let list: Arc<EpochSkipTrie> = Arc::new(EpochSkipTrie::with_universe_size(universe));
    let mut handles = vec![];

    for t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            let base = (t * ops_per_thread) as i64;
            for i in 0..ops_per_thread {
                list_clone.insert(base + i as i64);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn bench_concurrent_batch_insert_skiptrie(thread_count: usize, ops_per_thread: usize) {
    let universe = (thread_count * ops_per_thread).next_power_of_two();
    let list: Arc<EpochSkipTrie> = Arc::new(EpochSkipTrie::with_universe_size(universe));
    let mut handles = vec![];

    for t in 0..thread_count {
        let list_clone = Arc::clone(&list);
        let handle = thread::spawn(move || {
            let base = (t * ops_per_thread) as i64;
            let data: Vec<i64> = (base..base + ops_per_thread as i64).collect();
            let ordered = Ordered::new(data.into_iter());
            list_clone.insert_batch(ordered);
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

fn concurrent_batch_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_batch_insert_benchmark_sorted_collection");
    group.sample_size(10);
    let ops_per_thread = 5_000;

    for threads in thread_counts() {
        group.throughput(Throughput::Elements((threads * ops_per_thread) as u64));
        // SkipList
        group.bench_with_input(
            BenchmarkId::new(
                "concurrent_batch_insert_benchmark_skiplist_sequential",
                threads,
            ),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_concurrent_sequential_insert::<EpochSkipList>(
                        black_box(threads),
                        black_box(ops_per_thread),
                    )
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("concurrent_batch_insert_benchmark_skiplist_batch", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_concurrent_batch_insert::<EpochSkipList>(
                        black_box(threads),
                        black_box(ops_per_thread),
                    )
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new(
                "concurrent_batch_insert_benchmark_sorted_list_batch",
                threads,
            ),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_concurrent_batch_insert::<EpochSortedList>(
                        black_box(threads),
                        black_box(ops_per_thread),
                    )
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new(
                "concurrent_batch_insert_benchmark_skiptrie_sequential",
                threads,
            ),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_concurrent_sequential_insert_skiptrie(
                        black_box(threads),
                        black_box(ops_per_thread),
                    )
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("concurrent_batch_insert_benchmark_skiptrie_batch", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_concurrent_batch_insert_skiptrie(
                        black_box(threads),
                        black_box(ops_per_thread),
                    )
                })
            },
        );
    }

    group.finish();
}

// ============================================================================
// Update benchmarks - testing UPDATE_MARK approach
// ============================================================================

/// Sequential update benchmark - updates existing keys
fn bench_sorted_list_update(count: usize, update_iterations: usize) {
    let list: EpochSortedList = EpochSortedList::default();
    bench_update(&list, count, update_iterations);
}

/// Delete+Insert for comparison (traditional approach to update)
fn bench_sorted_list_delete_insert(count: usize, update_iterations: usize) {
    let list: EpochSortedList = EpochSortedList::default();
    bench_delete_insert(&list, count, update_iterations);
}

/// Concurrent update benchmark
fn bench_concurrent_update_sorted_list(thread_count: usize, ops_per_thread: usize) {
    let list: Arc<EpochSortedList> = Arc::new(EpochSortedList::default());
    bench_concurrent_update(list, thread_count, ops_per_thread);
}

/// Concurrent delete+insert benchmark for comparison
fn bench_concurrent_delete_insert_sorted_list(thread_count: usize, ops_per_thread: usize) {
    let list: Arc<EpochSortedList> = Arc::new(EpochSortedList::default());
    bench_concurrent_delete_insert(list, thread_count, ops_per_thread);
}

/// High contention update - all threads update same keys
fn bench_high_contention_update(thread_count: usize, ops_per_thread: usize) {
    let list: Arc<EpochSortedList> = Arc::new(EpochSortedList::default());
    bench_high_contention(list, thread_count, ops_per_thread);
}

/// Update benchmark group
fn update_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("update_benchmark_sorted_collection");
    group.sample_size(30);

    // Sequential update vs delete+insert
    for size in [100, 500, 1000] {
        group.throughput(Throughput::Elements((size * 10) as u64));
        group.bench_with_input(
            BenchmarkId::new("update_benchmark_sorted_list_update", size),
            &size,
            |b, &size| b.iter(|| bench_sorted_list_update(black_box(size), black_box(10))),
        );

        group.bench_with_input(
            BenchmarkId::new("update_benchmark_sorted_list_delete_insert", size),
            &size,
            |b, &size| b.iter(|| bench_sorted_list_delete_insert(black_box(size), black_box(10))),
        );
    }

    group.finish();
}

/// Concurrent update benchmark group
fn concurrent_update_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_update_benchmark_sorted_collection");
    group.sample_size(30);
    let ops_per_thread = 100;

    for threads in thread_counts() {
        group.throughput(Throughput::Elements((threads * ops_per_thread) as u64));
        group.bench_with_input(
            BenchmarkId::new("concurrent_update_benchmark_sorted_list", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_concurrent_update_sorted_list(
                        black_box(threads),
                        black_box(ops_per_thread),
                    )
                })
            },
        );

        // Only run delete+insert for single-threaded (concurrent delete+insert has edge case bug)
        if threads == 1 {
            group.bench_with_input(
                BenchmarkId::new("concurrent_update_benchmark_delete_insert", threads),
                &threads,
                |b, &threads| {
                    b.iter(|| {
                        bench_concurrent_delete_insert_sorted_list(
                            black_box(threads),
                            black_box(ops_per_thread),
                        )
                    })
                },
            );
        }
    }

    group.finish();
}

/// High contention update benchmark
fn contention_update_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention_update_benchmark_sorted_collection");
    group.sample_size(30);
    let ops_per_thread = 1000;

    for threads in thread_counts() {
        group.throughput(Throughput::Elements((threads * ops_per_thread) as u64));
        group.bench_with_input(
            BenchmarkId::new("contention_update_benchmark_sorted_list", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_high_contention_update(black_box(threads), black_box(ops_per_thread))
                })
            },
        );
    }

    group.finish();
}

// ============================================================================
// SkipList Update benchmarks
// ============================================================================

/// Sequential update benchmark for SkipList
fn bench_skiplist_update(count: usize, update_iterations: usize) {
    let list: EpochSkipList = EpochSkipList::default();
    bench_update(&list, count, update_iterations);
}

/// Delete+Insert for SkipList (traditional approach)
fn bench_skiplist_delete_insert(count: usize, update_iterations: usize) {
    let list: EpochSkipList = EpochSkipList::default();
    bench_delete_insert(&list, count, update_iterations);
}

/// Concurrent update benchmark for SkipList
fn bench_concurrent_update_skiplist(thread_count: usize, ops_per_thread: usize) {
    let list: Arc<EpochSkipList> = Arc::new(EpochSkipList::default());
    bench_concurrent_update(list, thread_count, ops_per_thread);
}

/// High contention update for SkipList
fn bench_high_contention_update_skiplist(thread_count: usize, ops_per_thread: usize) {
    let list: Arc<EpochSkipList> = Arc::new(EpochSkipList::default());
    bench_high_contention(list, thread_count, ops_per_thread);
}

/// SkipList update benchmark group
fn skiplist_update_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("skiplist_update_benchmark");
    group.sample_size(30);

    // Sequential update vs delete+insert
    for size in [100, 500, 1000] {
        group.throughput(Throughput::Elements((size * 10) as u64));
        group.bench_with_input(
            BenchmarkId::new("skiplist_update", size),
            &size,
            |b, &size| b.iter(|| bench_skiplist_update(black_box(size), black_box(10))),
        );

        group.bench_with_input(
            BenchmarkId::new("skiplist_delete_insert", size),
            &size,
            |b, &size| b.iter(|| bench_skiplist_delete_insert(black_box(size), black_box(10))),
        );
    }

    group.finish();
}

/// Concurrent SkipList update benchmark group
fn concurrent_skiplist_update_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_skiplist_update_benchmark");
    group.sample_size(30);
    let ops_per_thread = 100;

    for threads in thread_counts() {
        group.throughput(Throughput::Elements((threads * ops_per_thread) as u64));
        group.bench_with_input(
            BenchmarkId::new("concurrent_skiplist_update", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_concurrent_update_skiplist(black_box(threads), black_box(ops_per_thread))
                })
            },
        );
    }

    group.finish();
}

/// High contention SkipList update benchmark
fn contention_skiplist_update_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention_skiplist_update_benchmark");
    group.sample_size(30);
    let ops_per_thread = 1000;

    for threads in thread_counts() {
        group.throughput(Throughput::Elements((threads * ops_per_thread) as u64));
        group.bench_with_input(
            BenchmarkId::new("contention_skiplist_update", threads),
            &threads,
            |b, &threads| {
                b.iter(|| {
                    bench_high_contention_update_skiplist(
                        black_box(threads),
                        black_box(ops_per_thread),
                    )
                })
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    sequential_insert_benchmark,
    random_insert_benchmark,
    sequential_find_benchmark,
    random_find_benchmark,
    find_miss_benchmark,
    mixed_contention_sequential_benchmark,
    insert_benchmark,
    mixed_benchmark,
    contention_benchmark,
    batch_insert_benchmark,
    concurrent_batch_benchmark,
    skiplist_batch_benchmark,
    update_benchmark,
    concurrent_update_benchmark,
    contention_update_benchmark,
    skiplist_update_benchmark,
    concurrent_skiplist_update_benchmark,
    contention_skiplist_update_benchmark,
);
criterion_main!(benches);
