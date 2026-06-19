//! Benchmark comparing concurrent realistic workloads:
//! - SkipList, SkipTrie, Y-fast Trie, SplitOrderedHashMap, crossbeam-skiplist SkipSet, scc::TreeIndex
//!
//! Run with: cargo bench --package starfish-crossbeam --bench indexset_concurrent

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use crossbeam_skiplist::SkipSet;
use rand::{Rng, thread_rng};
use scc::TreeIndex;
use starfish_core::common_tests::bench_utils::max_threads;
use starfish_core::data_structures::SortedCollection;
use starfish_core::data_structures::SplitOrderedHashMap;
use starfish_core::data_structures::hash::MapCollection;
use starfish_core::data_structures::sorted::skip_list::SkipList;
use starfish_core::data_structures::trie::skip_trie::SkipTrie;
use starfish_core::data_structures::trie::y_fast_trie::YFastTrie;
use starfish_crossbeam::EpochGuard;
use std::sync::Arc;
use std::thread;

#[derive(Clone)]
enum Op {
    Read(usize),
    Write(usize),
}

const OPERATIONS_PER_THREAD: usize = 10_000;

struct ThreadConfig {
    num_threads: usize,
    num_writers: usize,
    total_operations: usize,
}

fn thread_config() -> ThreadConfig {
    let num_threads = max_threads().max(2);
    let num_writers = (num_threads / 4).max(1);
    let total_operations = num_threads * OPERATIONS_PER_THREAD;
    ThreadConfig {
        num_threads,
        num_writers,
        total_operations,
    }
}

fn generate_operations(write_ratio: f64, config: &ThreadConfig) -> Vec<Vec<Op>> {
    let mut rng = thread_rng();
    let mut all_operations = vec![Vec::with_capacity(OPERATIONS_PER_THREAD); config.num_threads];

    for (thread_idx, thread_operations) in all_operations
        .iter_mut()
        .enumerate()
        .take(config.num_threads)
    {
        let range_start = thread_idx * (config.total_operations / config.num_threads);
        let range_end = (thread_idx + 1) * (config.total_operations / config.num_threads);

        for _ in 0..OPERATIONS_PER_THREAD {
            let value = rng.r#gen_range(range_start..range_end);
            let operation = if thread_idx < config.num_writers || rng.r#gen::<f64>() < write_ratio {
                Op::Write(value)
            } else {
                Op::Read(value)
            };
            thread_operations.push(operation);
        }
    }
    all_operations
}

fn concurrent_operations<T: Send + Sync + 'static>(
    set: Arc<T>,
    operations: Vec<Op>,
    read_op: impl Fn(&T, usize) + Send + Sync + 'static,
    write_op: impl Fn(&T, usize) + Send + Sync + 'static,
) {
    for op in operations {
        match op {
            Op::Read(value) => read_op(&set, value),
            Op::Write(value) => write_op(&set, value),
        }
    }
}

fn bench_concurrent_sets_with_ratio(c: &mut Criterion, write_ratio: f64) {
    let config = thread_config();
    let operations = Arc::new(generate_operations(write_ratio, &config));

    let mut group = c.benchmark_group(format!(
        "Write Ratio: {:.2} ({}T)",
        write_ratio, config.num_threads
    ));
    group.warm_up_time(std::time::Duration::from_millis(500));
    group.measurement_time(std::time::Duration::from_millis(500));
    group.sample_size(10);
    group.throughput(Throughput::Elements(config.total_operations as u64));

    // Starfish SkipList
    group.bench_function(BenchmarkId::new("Starfish SkipList", write_ratio), |b| {
        b.iter(|| {
            let set = Arc::new(SkipList::new());
            let mut handles = vec![];

            for thread_ops in operations.iter() {
                let set = Arc::clone(&set);
                let thread_ops = thread_ops.clone();
                let handle = thread::spawn(move || {
                    concurrent_operations(
                        set,
                        thread_ops,
                        |set: &SkipList<usize, EpochGuard>, item| {
                            black_box(set.contains(&item));
                        },
                        |set: &SkipList<usize, EpochGuard>, item| {
                            black_box(set.insert(item));
                        },
                    );
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    // Starfish SkipTrie
    group.bench_function(BenchmarkId::new("Starfish SkipTrie", write_ratio), |b| {
        b.iter(|| {
            let set = Arc::new(SkipTrie::new());
            let mut handles = vec![];

            for thread_ops in operations.iter() {
                let set = Arc::clone(&set);
                let thread_ops = thread_ops.clone();
                let handle = thread::spawn(move || {
                    concurrent_operations(
                        set,
                        thread_ops,
                        |set: &SkipTrie<usize, EpochGuard>, item| {
                            black_box(set.contains(&item));
                        },
                        |set: &SkipTrie<usize, EpochGuard>, item| {
                            black_box(set.insert(item));
                        },
                    );
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    // Y-fast Trie
    group.bench_function(BenchmarkId::new("Y-fast Trie", write_ratio), |b| {
        b.iter(|| {
            let set = Arc::new(YFastTrie::<usize, usize, EpochGuard>::new());
            let mut handles = vec![];

            for thread_ops in operations.iter() {
                let set = Arc::clone(&set);
                let thread_ops = thread_ops.clone();
                let handle = thread::spawn(move || {
                    concurrent_operations(
                        set,
                        thread_ops,
                        |set: &YFastTrie<usize, usize, EpochGuard>, item| {
                            black_box(set.contains_key(&item));
                        },
                        |set: &YFastTrie<usize, usize, EpochGuard>, item| {
                            black_box(set.insert(item, item));
                        },
                    );
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    // SplitOrderedHashMap
    group.bench_function(BenchmarkId::new("SplitOrderedHashMap", write_ratio), |b| {
        b.iter(|| {
            let set = Arc::new(SplitOrderedHashMap::<usize, usize, EpochGuard>::new());
            let mut handles = vec![];

            for thread_ops in operations.iter() {
                let set = Arc::clone(&set);
                let thread_ops = thread_ops.clone();
                let handle = thread::spawn(move || {
                    concurrent_operations(
                        set,
                        thread_ops,
                        |set: &SplitOrderedHashMap<usize, usize, EpochGuard>, item| {
                            black_box(set.contains(&item));
                        },
                        |set: &SplitOrderedHashMap<usize, usize, EpochGuard>, item| {
                            black_box(set.insert(item, item));
                        },
                    );
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    // Crossbeam SkipSet
    group.bench_function(BenchmarkId::new("Crossbeam SkipSet", write_ratio), |b| {
        b.iter(|| {
            let set = Arc::new(SkipSet::new());
            let mut handles = vec![];

            for thread_ops in operations.iter() {
                let set = Arc::clone(&set);
                let thread_ops = thread_ops.clone();
                let handle = thread::spawn(move || {
                    concurrent_operations(
                        set,
                        thread_ops,
                        |set, item| {
                            black_box(set.contains(&item));
                        },
                        |set, item| {
                            black_box(set.insert(item));
                        },
                    );
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    // scc::TreeIndex
    group.bench_function(BenchmarkId::new("scc::TreeIndex", write_ratio), |b| {
        b.iter(|| {
            let set = Arc::new(TreeIndex::new());
            let mut handles = vec![];

            for thread_ops in operations.iter() {
                let set = Arc::clone(&set);
                let thread_ops = thread_ops.clone();
                let handle = thread::spawn(move || {
                    concurrent_operations(
                        set,
                        thread_ops,
                        |set, item| {
                            black_box(set.contains(&item));
                        },
                        |set, item| {
                            let _ = set.insert(item, ());
                            black_box(());
                        },
                    );
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    group.finish();
}

fn bench_concurrent_sets(c: &mut Criterion) {
    let ratios = vec![0.01, 0.1, 0.3, 0.5];
    for ratio in ratios {
        bench_concurrent_sets_with_ratio(c, ratio);
    }
}

criterion_group!(benches, bench_concurrent_sets);
criterion_main!(benches);
