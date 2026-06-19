//! Profiling-only binary for hash map collections.
//!
//! Runs a single data structure doing a single operation in a tight loop.
//! Attach an external profiler (samply, Instruments, perf, callgrind).
//!
//! Usage: profile_hashmap <n> <hashmap|skiplist-pair> <operation> [threads]
//!
//! Operations:
//!   seq-insert, seq-find-hit, seq-find-miss, seq-remove, seq-mixed,
//!   concurrent-insert, concurrent-mixed, concurrent-contention

use starfish_core::data_structures::SplitOrderedHashMap;
use starfish_core::data_structures::hash::MapCollection;
use starfish_core::data_structures::map_entry::MapEntry;
use starfish_core::data_structures::sorted::skip_list::SkipList;
use starfish_crossbeam::EpochGuard;
use std::hint::black_box;
use std::sync::Arc;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

type EpochHashMap<K, V> = SplitOrderedHashMap<K, V, EpochGuard>;
type EpochSkipMap<K, V> = SkipList<MapEntry<K, V>, EpochGuard>;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("Usage: profile_hashmap <n> <hashmap|skiplist-pair> <operation> [threads]");
        eprintln!("Operations: seq-insert, seq-find-hit, seq-find-miss, seq-remove, seq-mixed,");
        eprintln!("            concurrent-insert, concurrent-mixed, concurrent-contention");
        std::process::exit(1);
    }

    let n: usize = args[1].parse().expect("n must be an integer");
    let ds = &args[2];
    let op = &args[3];
    let threads: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);

    match (ds.as_str(), op.as_str()) {
        // SplitOrderedHashMap
        ("hashmap", "seq-insert") => {
            eprintln!("Profiling hashmap seq-insert ({n} elements)...");
            let map: EpochHashMap<usize, usize> = EpochHashMap::new();
            for i in 0..n {
                black_box(map.insert(i, i));
            }
        }
        ("hashmap", "seq-find-hit") => {
            let map: EpochHashMap<usize, usize> = EpochHashMap::new();
            for i in 0..n {
                map.insert(i, i);
            }
            eprintln!("Profiling hashmap seq-find-hit ({n} elements)...");
            for i in 0..n {
                black_box(map.contains(&i));
            }
        }
        ("hashmap", "seq-find-miss") => {
            let map: EpochHashMap<usize, usize> = EpochHashMap::new();
            for i in 0..n {
                map.insert(i, i);
            }
            eprintln!("Profiling hashmap seq-find-miss ({n} elements)...");
            for i in n..2 * n {
                black_box(map.contains(&i));
            }
        }
        ("hashmap", "seq-remove") => {
            let map: EpochHashMap<usize, usize> = EpochHashMap::new();
            for i in 0..n {
                map.insert(i, i);
            }
            eprintln!("Profiling hashmap seq-remove ({n} elements)...");
            for i in 0..n {
                black_box(map.remove(&i));
            }
        }
        ("hashmap", "seq-mixed") => {
            let map: EpochHashMap<usize, usize> = EpochHashMap::new();
            for i in 0..n / 2 {
                map.insert(i, i);
            }
            eprintln!("Profiling hashmap seq-mixed ({n} ops)...");
            for i in 0..n {
                match i % 3 {
                    0 => {
                        black_box(map.insert(i + n, i));
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
        }
        ("hashmap", "concurrent-insert") => {
            let ops = n / threads;
            eprintln!(
                "Profiling hashmap concurrent-insert ({threads} threads, {ops} ops/thread)..."
            );
            let map = Arc::new(EpochHashMap::<usize, usize>::new());
            std::thread::scope(|s| {
                for t in 0..threads {
                    let m = Arc::clone(&map);
                    s.spawn(move || {
                        let base = t * ops;
                        for i in 0..ops {
                            black_box(m.insert(base + i, i));
                        }
                    });
                }
            });
        }
        ("hashmap", "concurrent-mixed") => {
            let ops = n / threads;
            let map = Arc::new(EpochHashMap::<usize, usize>::new());
            for i in 0..ops {
                map.insert(i, i);
            }
            eprintln!(
                "Profiling hashmap concurrent-mixed ({threads} threads, {ops} ops/thread)..."
            );
            std::thread::scope(|s| {
                for t in 0..threads {
                    let m = Arc::clone(&map);
                    s.spawn(move || {
                        let base = t * ops;
                        for i in 0..ops {
                            match i % 4 {
                                0 | 1 => {
                                    black_box(m.insert(base + i + 1_000_000, i));
                                }
                                2 => {
                                    black_box(m.contains(&(i % ops)));
                                }
                                3 => {
                                    black_box(m.remove(&(base + i + 1_000_000)));
                                }
                                _ => unreachable!(),
                            }
                        }
                    });
                }
            });
        }
        ("hashmap", "concurrent-contention") => {
            let key_range = 100usize;
            let ops = n / threads;
            eprintln!(
                "Profiling hashmap concurrent-contention ({threads} threads, {ops} ops/thread)..."
            );
            let map = Arc::new(EpochHashMap::<usize, usize>::new());
            std::thread::scope(|s| {
                for _ in 0..threads {
                    let m = Arc::clone(&map);
                    s.spawn(move || {
                        for i in 0..ops {
                            let key = i % key_range;
                            if i % 2 == 0 {
                                black_box(m.insert(key, i));
                            } else {
                                black_box(m.remove(&key));
                            }
                        }
                    });
                }
            });
        }

        // SkipList<MapEntry> via MapCollection (value-CAS update)
        ("skiplist-pair", "seq-insert") => {
            eprintln!("Profiling skiplist-pair seq-insert ({n} elements)...");
            let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
            for i in 0..n {
                black_box(map.insert(i, i));
            }
        }
        ("skiplist-pair", "seq-find-hit") => {
            let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
            for i in 0..n {
                map.insert(i, i);
            }
            eprintln!("Profiling skiplist-pair seq-find-hit ({n} elements)...");
            for i in 0..n {
                black_box(map.contains(&i));
            }
        }
        ("skiplist-pair", "seq-find-miss") => {
            let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
            for i in 0..n {
                map.insert(i, i);
            }
            eprintln!("Profiling skiplist-pair seq-find-miss ({n} elements)...");
            for i in n..2 * n {
                black_box(map.contains(&i));
            }
        }
        ("skiplist-pair", "seq-remove") => {
            let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
            for i in 0..n {
                map.insert(i, i);
            }
            eprintln!("Profiling skiplist-pair seq-remove ({n} elements)...");
            for i in 0..n {
                black_box(map.remove(&i));
            }
        }
        ("skiplist-pair", "seq-mixed") => {
            let map: EpochSkipMap<usize, usize> = EpochSkipMap::new();
            for i in 0..n / 2 {
                map.insert(i, i);
            }
            eprintln!("Profiling skiplist-pair seq-mixed ({n} ops)...");
            for i in 0..n {
                match i % 3 {
                    0 => {
                        black_box(map.insert(i + n, i));
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
        }
        ("skiplist-pair", "concurrent-insert") => {
            let ops = n / threads;
            eprintln!(
                "Profiling skiplist-pair concurrent-insert ({threads} threads, {ops} ops/thread)..."
            );
            let map = Arc::new(EpochSkipMap::<usize, usize>::new());
            std::thread::scope(|s| {
                for t in 0..threads {
                    let m = Arc::clone(&map);
                    s.spawn(move || {
                        let base = t * ops;
                        for i in 0..ops {
                            black_box(m.insert(base + i, i));
                        }
                    });
                }
            });
        }
        ("skiplist-pair", "concurrent-mixed") => {
            let ops = n / threads;
            let map = Arc::new(EpochSkipMap::<usize, usize>::new());
            for i in 0..ops {
                map.insert(i, i);
            }
            eprintln!(
                "Profiling skiplist-pair concurrent-mixed ({threads} threads, {ops} ops/thread)..."
            );
            std::thread::scope(|s| {
                for t in 0..threads {
                    let m = Arc::clone(&map);
                    s.spawn(move || {
                        let base = t * ops;
                        for i in 0..ops {
                            match i % 4 {
                                0 | 1 => {
                                    black_box(m.insert(base + i + 1_000_000, i));
                                }
                                2 => {
                                    black_box(m.contains(&(i % ops)));
                                }
                                3 => {
                                    black_box(m.remove(&(base + i + 1_000_000)));
                                }
                                _ => unreachable!(),
                            }
                        }
                    });
                }
            });
        }
        ("skiplist-pair", "concurrent-contention") => {
            let key_range = 100usize;
            let ops = n / threads;
            eprintln!(
                "Profiling skiplist-pair concurrent-contention ({threads} threads, {ops} ops/thread)..."
            );
            let map = Arc::new(EpochSkipMap::<usize, usize>::new());
            std::thread::scope(|s| {
                for _ in 0..threads {
                    let m = Arc::clone(&map);
                    s.spawn(move || {
                        for i in 0..ops {
                            let key = i % key_range;
                            if i % 2 == 0 {
                                black_box(m.insert(key, i));
                            } else {
                                black_box(m.remove(&key));
                            }
                        }
                    });
                }
            });
        }

        _ => {
            eprintln!("Unknown: {ds} {op}");
            eprintln!("Data structures: hashmap, skiplist-pair");
            eprintln!(
                "Operations: seq-insert, seq-find-hit, seq-find-miss, seq-remove, seq-mixed,"
            );
            eprintln!("            concurrent-insert, concurrent-mixed, concurrent-contention");
            std::process::exit(1);
        }
    }

    eprintln!("Done.");
}
