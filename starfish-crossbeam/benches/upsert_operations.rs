//! Benchmark comparing concurrent update operations:
//! - SkipList, SkipTrie vs crossbeam-skiplist (delete+reinsert)
//!
//! Run with: cargo bench --package starfish-crossbeam --bench upsert_operations
//!
//! (Named "upsert", not "update": Windows UAC Installer Detection flags unsigned
//! executables whose names contain "update"/"setup"/"install"/"patch" as installers
//! and refuses to run them without elevation — os error 740 from a bare
//! `update_operations-<hash>.exe`.)

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use crossbeam_skiplist::SkipSet;
use rand::{Rng, thread_rng};
use starfish_core::common_tests::bench_utils::max_threads;
use starfish_core::data_structures::SortedCollection;
use starfish_core::data_structures::sorted::skip_list::SkipList;
use starfish_core::data_structures::trie::skip_trie::SkipTrie;
use starfish_crossbeam::EpochGuard;
use std::sync::Arc;
use std::thread;

const OPERATIONS_PER_THREAD: usize = 100_000;
const INITIAL_SIZE: usize = 500_000;

fn num_threads() -> usize {
    max_threads().max(2)
}

fn bench_concurrent_updates(c: &mut Criterion) {
    let num_threads = num_threads();
    let total_operations = num_threads * OPERATIONS_PER_THREAD;
    let thread_label = format!("{} threads", num_threads);

    let mut group = c.benchmark_group(format!("Concurrent Updates ({}T)", num_threads));
    group.warm_up_time(std::time::Duration::from_millis(500));
    group.measurement_time(std::time::Duration::from_secs(3));
    group.sample_size(10);
    group.throughput(Throughput::Elements(total_operations as u64));

    // Starfish SkipList
    group.bench_function(BenchmarkId::new("Starfish SkipList", &thread_label), |b| {
        b.iter(|| {
            let set: Arc<SkipList<usize, EpochGuard>> = Arc::new(SkipList::new());

            // Pre-populate
            for i in 0..INITIAL_SIZE {
                set.insert(i);
            }

            let mut handles = vec![];

            for _ in 0..num_threads {
                let set = Arc::clone(&set);
                let handle = thread::spawn(move || {
                    let mut rng = thread_rng();
                    for _ in 0..OPERATIONS_PER_THREAD {
                        let key = rng.r#gen_range(0..INITIAL_SIZE);
                        black_box(set.update(key));
                    }
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    // Starfish SkipTrie
    group.bench_function(BenchmarkId::new("Starfish SkipTrie", &thread_label), |b| {
        b.iter(|| {
            let set: Arc<SkipTrie<usize, EpochGuard>> = Arc::new(SkipTrie::new());

            // Pre-populate
            for i in 0..INITIAL_SIZE {
                set.insert(i);
            }

            let mut handles = vec![];

            for _ in 0..num_threads {
                let set = Arc::clone(&set);
                let handle = thread::spawn(move || {
                    let mut rng = thread_rng();
                    for _ in 0..OPERATIONS_PER_THREAD {
                        let key = rng.r#gen_range(0..INITIAL_SIZE);
                        black_box(set.update(key));
                    }
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    // Crossbeam SkipSet (remove + insert as update)
    group.bench_function(BenchmarkId::new("Crossbeam SkipSet", &thread_label), |b| {
        b.iter(|| {
            let set = Arc::new(SkipSet::new());

            // Pre-populate
            for i in 0..INITIAL_SIZE {
                set.insert(i);
            }

            let mut handles = vec![];

            for _ in 0..num_threads {
                let set = Arc::clone(&set);
                let handle = thread::spawn(move || {
                    let mut rng = thread_rng();
                    for _ in 0..OPERATIONS_PER_THREAD {
                        let key = rng.r#gen_range(0..INITIAL_SIZE);
                        // Crossbeam doesn't have update, so remove + insert
                        set.remove(&key);
                        black_box(set.insert(key));
                    }
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

fn bench_mixed_update_read(c: &mut Criterion) {
    let num_threads = num_threads();
    let total_operations = num_threads * OPERATIONS_PER_THREAD;

    let mut group = c.benchmark_group(format!("Mixed Update_Read ({}T)", num_threads));
    group.warm_up_time(std::time::Duration::from_millis(500));
    group.measurement_time(std::time::Duration::from_secs(3));
    group.sample_size(10);
    group.throughput(Throughput::Elements(total_operations as u64));

    let update_ratios = vec![0.01, 0.1, 0.5];

    for ratio in update_ratios {
        // Starfish SkipList
        group.bench_function(
            BenchmarkId::new(
                "Starfish SkipList",
                format!("{:.0}% updates", ratio * 100.0),
            ),
            |b| {
                b.iter(|| {
                    let set: Arc<SkipList<usize, EpochGuard>> = Arc::new(SkipList::new());

                    // Pre-populate
                    for i in 0..INITIAL_SIZE {
                        set.insert(i);
                    }

                    let mut handles = vec![];

                    for _ in 0..num_threads {
                        let set = Arc::clone(&set);
                        let handle = thread::spawn(move || {
                            let mut rng = thread_rng();
                            for _ in 0..OPERATIONS_PER_THREAD {
                                let key = rng.r#gen_range(0..INITIAL_SIZE);
                                if rng.r#gen::<f64>() < ratio {
                                    black_box(set.update(key));
                                } else {
                                    black_box(set.contains(&key));
                                }
                            }
                        });
                        handles.push(handle);
                    }

                    for handle in handles {
                        handle.join().unwrap();
                    }
                });
            },
        );

        // Starfish SkipTrie
        group.bench_function(
            BenchmarkId::new(
                "Starfish SkipTrie",
                format!("{:.0}% updates", ratio * 100.0),
            ),
            |b| {
                b.iter(|| {
                    let set: Arc<SkipTrie<usize, EpochGuard>> = Arc::new(SkipTrie::new());

                    // Pre-populate
                    for i in 0..INITIAL_SIZE {
                        set.insert(i);
                    }

                    let mut handles = vec![];

                    for _ in 0..num_threads {
                        let set = Arc::clone(&set);
                        let handle = thread::spawn(move || {
                            let mut rng = thread_rng();
                            for _ in 0..OPERATIONS_PER_THREAD {
                                let key = rng.r#gen_range(0..INITIAL_SIZE);
                                if rng.r#gen::<f64>() < ratio {
                                    black_box(set.update(key));
                                } else {
                                    black_box(set.contains(&key));
                                }
                            }
                        });
                        handles.push(handle);
                    }

                    for handle in handles {
                        handle.join().unwrap();
                    }
                });
            },
        );

        // Crossbeam SkipSet
        group.bench_function(
            BenchmarkId::new(
                "Crossbeam SkipSet",
                format!("{:.0}% updates", ratio * 100.0),
            ),
            |b| {
                b.iter(|| {
                    let set = Arc::new(SkipSet::new());

                    // Pre-populate
                    for i in 0..INITIAL_SIZE {
                        set.insert(i);
                    }

                    let mut handles = vec![];

                    for _ in 0..num_threads {
                        let set = Arc::clone(&set);
                        let handle = thread::spawn(move || {
                            let mut rng = thread_rng();
                            for _ in 0..OPERATIONS_PER_THREAD {
                                let key = rng.r#gen_range(0..INITIAL_SIZE);
                                if rng.r#gen::<f64>() < ratio {
                                    // Crossbeam: remove + insert
                                    set.remove(&key);
                                    black_box(set.insert(key));
                                } else {
                                    black_box(set.contains(&key));
                                }
                            }
                        });
                        handles.push(handle);
                    }

                    for handle in handles {
                        handle.join().unwrap();
                    }
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_concurrent_updates, bench_mixed_update_read);
criterion_main!(benches);
